//! A Debug Adapter Protocol (DAP) server for the Nulang bytecode VM.
//!
//! Started with `nulang --dap [file]`, it speaks DAP over stdio (the same
//! `Content-Length`-framed JSON-RPC as the LSP) so editors such as VS Code
//! can debug `.nula` programs with breakpoints, stepping, stack traces,
//! scopes, and local-variable inspection.
//!
//! # Architecture
//!
//! ```text
//!   DAP client (editor) <--stdio--> server loop <--> debuggee thread
//!                                       ^              |
//!                                       |  events      |  commands
//!                                       +------> control<--+
//! ```
//!
//! - A **reader thread** parses `Content-Length`-framed requests from stdin
//!   and forwards them to the server loop over a crossbeam channel, so the
//!   loop never blocks on stdin while the debuggee is paused.
//! - The **server loop** (main thread) dispatches requests, writes
//!   responses/events to stdout, and fans out debugger control
//!   (`continue`/`next`/`stepIn`/`stepOut`/`pause`) to the debuggee.
//! - The **debuggee thread** owns the [`VM`]. It installs a [`DebugHook`]
//!   that checks breakpoints/stepping before every interpreted instruction;
//!   a pause makes `VM::step` return the [`DEBUG_PAUSE_MSG`] sentinel, which
//!   the debuggee catches, reports as a `stopped` event, and waits for a
//!   resume command. `stackTrace`/`scopes`/`variables`/`evaluate` are
//!   answered by snapshotting the paused VM on the debuggee thread.
//!
//! # Scope (v1)
//!
//! The debuggee runs on the **standalone VM** (no actor runtime): top-level
//! code, functions, closures, and effect handlers (including `IO.print` /
//! `IO.read`) are fully debuggable; actor `spawn`/`send`/`receive` are
//! no-ops, matching the standalone VM's outside-an-actor contract. Program
//! stdout is captured and forwarded as `output` events so it never corrupts
//! the DAP stream. Stepping is **statement**-granular (via the compiler's
//! pc↔line table), not instruction-granular.
//!
//! To test the adapter in-process without a real editor, drive
//! [`run_dap_server_io`] with byte buffers and assert on the framed output.

pub mod rewind;

use crate::bytecode::{CodeModule, OpCode};
use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::runtime::PersistenceStore;
use crate::typechecker::TypeChecker;
use crate::types::NuResult;
use crate::vm::{DebugAction, DebugContext, DebugHook, Value, VM};
use crossbeam::channel::{Receiver, Sender};
use parking_lot::Mutex;
use serde_json::{json, Value as Json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Message framing (DAP uses the same Content-Length framing as the LSP)
// ---------------------------------------------------------------------------

fn read_message<R: BufRead>(reader: &mut R) -> Option<Json> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_message<W: Write>(writer: &mut W, msg: &Json) {
    let body = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string());
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = writer.flush();
}

// ---------------------------------------------------------------------------
// Debuggee control state (shared between the server loop and the debug hook)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepMode {
    Run,
    StepIn,
    StepOver,
    StepOut,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedBreakpoint {
    pc: usize,
}

/// Mutable state consulted by the debug hook on every instruction and
/// mutated by the server loop between pauses.
struct ControlState {
    breakpoints: Vec<ResolvedBreakpoint>,
    /// Set by the server's `pause` request; consumed on the next checkpoint.
    pause_requested: bool,
    /// `stopOnEntry` launch: the next pause (from `pause_requested`) reports
    /// reason "entry" instead of "pause".
    entry_pause: bool,
    /// Active stepping mode (set when a step command resumes execution).
    step: Option<StepMode>,
    /// Frame depth / line recorded at the last pause; targets for stepping.
    step_target_depth: usize,
    step_target_line: Option<u32>,
    /// PC at the last pause. The hook skips breakpoint/step checks for one
    /// instruction at this pc after a resume so a paused-at breakpoint does
    /// not immediately re-trigger.
    resume_pc: Option<usize>,
    /// Reason for the most recent `stopped` event ("breakpoint"/"step"/"pause").
    last_reason: String,
}

impl ControlState {
    fn new() -> Self {
        ControlState {
            breakpoints: Vec::new(),
            pause_requested: false,
            entry_pause: false,
            step: None,
            step_target_depth: 0,
            step_target_line: None,
            resume_pc: None,
            last_reason: "breakpoint".to_string(),
        }
    }
}

/// The `DebugHook` implementation driving breakpoints and stepping.
struct DapDebugger {
    control: Arc<Mutex<ControlState>>,
}

impl DebugHook for DapDebugger {
    fn before_instruction(&mut self, ctx: &DebugContext) -> DebugAction {
        let mut c = self.control.lock();
        // User pause takes priority.
        if c.pause_requested {
            c.pause_requested = false;
            if c.entry_pause {
                c.entry_pause = false;
                c.last_reason = "entry".to_string();
            } else {
                c.last_reason = "pause".to_string();
            }
            return DebugAction::Pause;
        }
        let at_resume_pc = c.resume_pc == Some(ctx.pc);
        // Breakpoints (skipped for the instruction we resumed from).
        if !at_resume_pc
            && (c.breakpoints.iter().any(|b| b.pc == ctx.pc) || ctx.opcode == OpCode::DbgBreak)
        {
            c.last_reason = "breakpoint".to_string();
            return DebugAction::Pause;
        }
        // Stepping modes.
        let line_changed = match c.step_target_line {
            Some(tl) => ctx.line.is_some() && ctx.line != Some(tl),
            None => ctx.line.is_some(),
        };
        let step_hit = match c.step {
            Some(StepMode::StepIn) => !at_resume_pc,
            Some(StepMode::StepOver) => {
                !at_resume_pc && ctx.frame_depth <= c.step_target_depth && line_changed
            }
            Some(StepMode::StepOut) => !at_resume_pc && ctx.frame_depth < c.step_target_depth,
            _ => false,
        };
        if step_hit {
            c.step = None;
            c.last_reason = "step".to_string();
            return DebugAction::Pause;
        }
        if at_resume_pc {
            c.resume_pc = None;
        }
        DebugAction::Continue
    }
}

// ---------------------------------------------------------------------------
// Events and commands exchanged between the debuggee thread and the server
// ---------------------------------------------------------------------------

enum DebugEvent {
    Stopped { reason: String, thread_id: i64 },
    Continued { thread_id: i64 },
    Output { category: String, message: String },
    Exited { exit_code: i64 },
    Terminated,
}

enum DebugCommand {
    Start,
    Resume,
    Terminate,
    GetState(Sender<Json>),
}

// ---------------------------------------------------------------------------
// Compilation (replicates `main::run_frontend` for the lib crate)
// ---------------------------------------------------------------------------

fn compile_source(source: &str, file_path: Option<&str>, name: &str) -> NuResult<CodeModule> {
    // 1. Prelude (variant-type declarations only) + main source.
    let ps = crate::prelude_source::PRELUDE_SOURCE;
    let mut pl = Lexer::new(ps);
    crate::types::set_source_map_with_file(ps, Some("<prelude>"));
    let pt = pl.lex()?;
    let mut pp = Parser::new(pt);
    let pa = pp.parse_module()?;

    let mut lexer = Lexer::new(source);
    crate::types::set_source_map_with_file(source, file_path);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let mut ast = parser.parse_module()?;
    let pd: Vec<crate::ast::Decl> = pa
        .decls
        .into_iter()
        .filter(|d| matches!(d, crate::ast::Decl::VariantType { .. }))
        .collect();

    // 2. Import resolution.
    let mut visited = HashSet::new();
    crate::resolver::resolve_imports(
        &mut ast,
        std::path::Path::new(file_path.unwrap_or(".")),
        &mut visited,
    )?;

    // Prepend the prelude AFTER import resolution (see main::run_frontend):
    // `resolve_imports` prepends imported declarations, and the typechecker
    // binds variant constructors in declaration order, so the prelude's
    // `Option`/`Result` declarations must come first in the final AST.
    let mut pd = pd;
    pd.append(&mut ast.decls);
    ast.decls = pd;

    // 3. Type check.
    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast)?;

    // 4. Effect check.
    let mut effect_checker = EffectChecker::new();
    effect_checker.check_module(&ast.decls)?;

    // 5. Capability analysis (mirrors main.rs's loop over bodies).
    let flat_decls = crate::effect_checker::flatten_decls(&ast.decls);
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_body = |analyzer: &mut CapabilityAnalyzer,
                    ctx: &CapContext,
                    body: &crate::ast::Expr|
     -> NuResult<()> { analyzer.infer_cap(ctx, body).map(|_| ()) };
    let seed_from_params = |ctx: &mut CapContext, params: &[crate::ast::Param]| {
        for p in params {
            if let Some(c) = p.cap {
                *ctx = ctx.clone().with_binding(&p.name, c);
            }
        }
    };
    for decl in flat_decls.iter().copied() {
        match decl {
            crate::ast::Decl::Function { body, params, .. } => {
                let mut ctx = CapContext::new();
                seed_from_params(&mut ctx, params);
                cap_body(&mut cap_analyzer, &ctx, body)?;
            }
            crate::ast::Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for b in behaviors {
                    let mut ctx = CapContext::new();
                    seed_from_params(&mut ctx, &b.params);
                    cap_body(&mut cap_analyzer, &ctx, &b.body)?;
                }
                for (_, _, _, default) in state_fields {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, default)?;
                }
                for (_, expr) in init {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, expr)?;
                }
            }
            crate::ast::Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    let steps: &[crate::ast::WorkflowStep] = match item {
                        crate::ast::WorkflowItem::Step(s) => std::slice::from_ref(s),
                        crate::ast::WorkflowItem::Parallel(steps) => steps,
                    };
                    for step in steps {
                        let ctx = CapContext::new();
                        cap_body(&mut cap_analyzer, &ctx, &step.body)?;
                        if let Some(comp) = &step.compensate {
                            cap_body(&mut cap_analyzer, &ctx, comp)?;
                        }
                    }
                }
                if let Some(comp) = compensate {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, comp)?;
                }
            }
            _ => {}
        }
    }

    // 6. Lower and compile.
    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir)?;
    crate::mir_codegen::compile_mir(&mut mir, name)
}

// ---------------------------------------------------------------------------
// Snapshot / value formatting
// ---------------------------------------------------------------------------

fn fmt_value(v: &Value, vm: &VM, module_idx: usize) -> String {
    if let Some(i) = v.as_int() {
        return format!("{}", i);
    }
    if let Some(f) = v.as_float() {
        return format!("{}", f);
    }
    if v.is_bool() {
        return format!("{}", v.as_raw() != 0);
    }
    if v.is_nil() {
        return "nil".to_string();
    }
    if v.is_unit() {
        return "unit".to_string();
    }
    if v.is_string() || v.is_ptr() {
        if let Some(s) = vm.string_operand(module_idx, *v) {
            return format!("\"{}\"", s);
        }
        return "<string?>".to_string();
    }
    if v.is_closure() {
        return "<closure>".to_string();
    }
    if v.is_actor_ref() {
        return "<actor>".to_string();
    }
    "<value>".to_string()
}

fn debug_fn_for<'a>(
    module: &'a CodeModule,
    pc: usize,
) -> Option<&'a crate::bytecode::DebugFunctionInfo> {
    module
        .debug_functions
        .iter()
        .find(|df| pc >= df.code_offset && pc < df.code_offset + df.code_len)
}

fn build_snapshot(vm: &VM, program: &str) -> Json {
    let mut frames = Vec::new();
    let Some(cur) = vm.current_frame_index() else {
        return json!({ "frames": [] });
    };
    // Walk the caller chain, current (innermost) frame first — DAP orders
    // stackFrames top-of-stack first. Anonymous VM-plumbing frames (e.g. the
    // synthetic top-level runner, which has no debug-functions entry) are
    // skipped so only user code frames are reported.
    let mut chain = Vec::new();
    let mut idx = Some(cur);
    while let Some(i) = idx {
        chain.push(i);
        idx = vm.frames().get(i).and_then(|f| f.caller_idx);
    }
    for (id, fi) in chain.iter().enumerate() {
        let Some(f) = vm.frames().get(*fi) else {
            continue;
        };
        let module = vm.modules().get(f.module_idx);
        let Some(df) = module.and_then(|m| debug_fn_for(m, f.pc)) else {
            continue; // anonymous VM-plumbing frame
        };
        let name = df.name.clone();
        let line = module.and_then(|m| m.line_at(f.pc));
        // Named locals for this frame (its registers hold its own function's
        // locals; debug_functions maps pc -> local names).
        let mut locals = Vec::new();
        for &(reg, ref lname) in &df.locals {
            if let Some(lname) = lname {
                if reg < 256 {
                    locals.push(json!([lname, fmt_value(&f.regs[reg], vm, f.module_idx)]));
                }
            }
        }
        // Non-nil registers for the active frame (Registers scope).
        let mut registers = Vec::new();
        if *fi == cur {
            for (ri, v) in f.regs.iter().enumerate() {
                if !v.is_nil() {
                    registers.push(json!([format!("r{}", ri), fmt_value(v, vm, f.module_idx)]));
                }
            }
        }
        frames.push(json!({
            "id": id,
            "name": name,
            "moduleIdx": f.module_idx,
            "pc": f.pc,
            "line": line,
            "locals": locals,
            "registers": registers,
            "source": program,
        }));
    }
    json!({ "frames": frames })
}

// ---------------------------------------------------------------------------
// The debuggee thread
// ---------------------------------------------------------------------------

fn drain_output(out_buf: &Rc<RefCell<Vec<String>>>, to_server: &Sender<DebugEvent>) {
    let mut buf = out_buf.borrow_mut();
    if buf.is_empty() {
        return;
    }
    let joined = std::mem::take(&mut *buf).concat();
    let _ = to_server.send(DebugEvent::Output {
        category: "stdout".to_string(),
        message: joined,
    });
}

fn debuggee_main(
    module: CodeModule,
    program: String,
    control: Arc<Mutex<ControlState>>,
    to_server: Sender<DebugEvent>,
    from_server: Receiver<DebugCommand>,
) {
    let out_buf: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut vm = VM::new();
    vm.set_output_capture(Some(out_buf.clone()));
    vm.set_io_output(Some(out_buf.clone()));
    vm.set_debug_hook(Some(Box::new(DapDebugger {
        control: control.clone(),
    })));
    vm.load_module(module);

    // Wait for `configurationDone` (the server sends Start).
    loop {
        match from_server.recv() {
            Ok(DebugCommand::Start) => break,
            Ok(DebugCommand::Terminate) => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let mut result = vm.run();
    loop {
        match result {
            Ok(_v) => {
                drain_output(&out_buf, &to_server);
                let _ = to_server.send(DebugEvent::Exited { exit_code: 0 });
                let _ = to_server.send(DebugEvent::Terminated);
                return;
            }
            Err(e) if crate::vm::is_debug_pause(&e) => {
                drain_output(&out_buf, &to_server);
                let reason = {
                    let mut c = control.lock();
                    let reason = c.last_reason.clone();
                    c.resume_pc = vm.current_pc();
                    c.step_target_depth = {
                        let mut d = 0;
                        let mut idx = vm.current_frame_index();
                        while let Some(x) = idx {
                            d += 1;
                            idx = vm.frames().get(x).and_then(|f| f.caller_idx);
                        }
                        d
                    };
                    c.step_target_line = vm.current_line();
                    reason
                };
                let _ = to_server.send(DebugEvent::Stopped {
                    reason,
                    thread_id: 1,
                });
                // Paused: serve state requests until resumed/terminated.
                let mut resume = false;
                while !resume {
                    match from_server.recv() {
                        Ok(DebugCommand::Resume) => resume = true,
                        Ok(DebugCommand::GetState(reply)) => {
                            let snap = build_snapshot(&vm, &program);
                            let _ = reply.send(snap);
                        }
                        Ok(DebugCommand::Terminate) => return,
                        Ok(DebugCommand::Start) => {}
                        Err(_) => return,
                    }
                }
                let _ = to_server.send(DebugEvent::Continued { thread_id: 1 });
                result = vm.resume();
            }
            Err(e) => {
                drain_output(&out_buf, &to_server);
                let _ = to_server.send(DebugEvent::Output {
                    category: "stderr".to_string(),
                    message: format!("{}\n", e),
                });
                let _ = to_server.send(DebugEvent::Exited { exit_code: 1 });
                let _ = to_server.send(DebugEvent::Terminated);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The server loop
// ---------------------------------------------------------------------------

fn write_event<W: Write>(writer: &mut W, seq: &mut i64, event: DebugEvent) {
    *seq += 1;
    let msg = match event {
        DebugEvent::Stopped { reason, thread_id } => json!({
            "seq": *seq,
            "type": "event",
            "event": "stopped",
            "body": { "reason": reason, "threadId": thread_id, "allThreadsStopped": true }
        }),
        DebugEvent::Continued { thread_id } => json!({
            "seq": *seq,
            "type": "event",
            "event": "continued",
            "body": { "threadId": thread_id, "allThreadsContinued": true }
        }),
        DebugEvent::Output { category, message } => json!({
            "seq": *seq,
            "type": "event",
            "event": "output",
            "body": { "category": category, "output": message }
        }),
        DebugEvent::Exited { exit_code } => json!({
            "seq": *seq,
            "type": "event",
            "event": "exited",
            "body": { "exitCode": exit_code }
        }),
        DebugEvent::Terminated => json!({
            "seq": *seq,
            "type": "event",
            "event": "terminated"
        }),
    };
    write_message(writer, &msg);
}

fn respond<W: Write>(
    writer: &mut W,
    seq: &mut i64,
    request_seq: i64,
    command: &str,
    body: Result<Json, String>,
) {
    *seq += 1;
    let msg = match body {
        Ok(b) => json!({
            "seq": *seq,
            "type": "response",
            "request_seq": request_seq,
            "success": true,
            "command": command,
            "body": b
        }),
        Err(message) => json!({
            "seq": *seq,
            "type": "response",
            "request_seq": request_seq,
            "success": false,
            "command": command,
            "message": message,
            "body": { "error": { "id": 1, "format": message } }
        }),
    };
    write_message(writer, &msg);
}

fn capabilities(rewind: &RewindSession) -> Json {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsEvaluate": true,
        "supportsTerminateRequest": true,
        "supportTerminateDebuggee": true,
        "supportsRestartRequest": false,
        // Time-travel rewind (Wave E3): only advertised when the durable
        // store is enabled — with the in-memory store there is nothing to
        // rewind. Dev/staging only.
        "supportsReverseContinueRequest": rewind.store_enabled(),
        "supportsStepBack": rewind.store_enabled()
    })
}

// ---------------------------------------------------------------------------
// Time-travel rewind session state (see `rewind` module)
// ---------------------------------------------------------------------------

/// Server-side state for reverse-continue / step-back over durable entities.
struct RewindSession {
    /// Durable store directory; `None` = store not enabled (rewind gated off).
    store_dir: Option<std::path::PathBuf>,
    /// Current rewind position per entity (actor id -> sequence). Absent =
    /// positioned at the latest message.
    positions: std::collections::HashMap<u64, u64>,
}

impl RewindSession {
    fn new(store_dir: Option<std::path::PathBuf>) -> Self {
        RewindSession {
            store_dir,
            positions: std::collections::HashMap::new(),
        }
    }

    fn store_enabled(&self) -> bool {
        self.store_dir.is_some()
    }

    fn open_store(&self) -> Option<crate::runtime::JsonFileStore> {
        self.store_dir.as_deref().and_then(rewind::open_store_at)
    }

    /// Rewind entity `actor_id` to message `target_seq` and remember the
    /// position so `stepBack` / `nulangStepForward` are relative to it.
    fn rewind_to(&mut self, actor_id: u64, target_seq: u64) -> Result<Json, String> {
        let store = self.open_store().ok_or_else(|| {
            "rewind requires a durable store ($NULANG_STORE_PATH or .nulang/store/)".to_string()
        })?;
        let state = rewind::rewind_entity(&store, actor_id, target_seq);
        self.positions.insert(actor_id, state.sequence);
        Ok(state.to_json())
    }

    /// Step back one message from the current position (default: latest).
    fn step_back(&mut self, actor_id: u64) -> Result<Json, String> {
        let store = self.open_store().ok_or_else(|| {
            "rewind requires a durable store ($NULANG_STORE_PATH or .nulang/store/)".to_string()
        })?;
        let current = self
            .positions
            .get(&actor_id)
            .copied()
            .unwrap_or_else(|| store.latest_sequence(actor_id));
        let state = rewind::rewind_entity(&store, actor_id, current.saturating_sub(1));
        self.positions.insert(actor_id, state.sequence);
        Ok(state.to_json())
    }

    /// Step forward one message from the current rewound position: replays
    /// the recorded events for the next sequence number.
    fn step_forward(&mut self, actor_id: u64) -> Result<Json, String> {
        let store = self.open_store().ok_or_else(|| {
            "rewind requires a durable store ($NULANG_STORE_PATH or .nulang/store/)".to_string()
        })?;
        let current = self
            .positions
            .get(&actor_id)
            .copied()
            .unwrap_or_else(|| store.latest_sequence(actor_id));
        let state = rewind::rewind_entity(&store, actor_id, current.saturating_add(1));
        self.positions.insert(actor_id, state.sequence);
        Ok(state.to_json())
    }
}

/// Ask the debuggee thread for a paused-state snapshot, waiting up to 2s so
/// a request sent while the program is *running* fails cleanly instead of
/// blocking the server loop forever (editors only query state while paused).
fn request_snapshot(cmd_tx: &Sender<DebugCommand>) -> Result<Json, String> {
    let (reply_tx, reply_rx) = crossbeam::channel::bounded(1);
    if cmd_tx.send(DebugCommand::GetState(reply_tx)).is_err() {
        return Err("debuggee unavailable".to_string());
    }
    match reply_rx.recv_timeout(std::time::Duration::from_millis(2000)) {
        Ok(snap) => Ok(snap),
        Err(_) => Err("debuggee is not paused".to_string()),
    }
}

fn spawn_debuggee(
    module: Arc<CodeModule>,
    program: String,
    stop_on_entry: bool,
    control: Arc<Mutex<ControlState>>,
    cmd_rx: Receiver<DebugCommand>,
    event_tx: &Sender<DebugEvent>,
) -> std::thread::JoinHandle<()> {
    if stop_on_entry {
        control.lock().pause_requested = true;
        control.lock().entry_pause = true;
    }
    let module = (*module).clone();
    let control = control.clone();
    let cmd_rx = cmd_rx.clone();
    let event_tx = event_tx.clone();
    std::thread::Builder::new()
        .name("nulang-dap-debuggee".to_string())
        .spawn(move || debuggee_main(module, program, control, event_tx, cmd_rx))
        .expect("failed to spawn debuggee thread")
}

fn handle_request<W: Write>(
    writer: &mut W,
    seq: &mut i64,
    request_seq: i64,
    command: &str,
    args: &Json,
    control: &Arc<Mutex<ControlState>>,
    debuggee: &mut Option<std::thread::JoinHandle<()>>,
    module: &mut Option<Arc<CodeModule>>,
    cmd_tx: &Sender<DebugCommand>,
    cmd_rx: &Receiver<DebugCommand>,
    event_tx: &Sender<DebugEvent>,
    rewind: &mut RewindSession,
) {
    match command {
        "initialize" => respond(writer, seq, request_seq, command, Ok(capabilities(rewind))),
        "launch" => {
            let program = args
                .get("program")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let Some(program) = program else {
                respond(
                    writer,
                    seq,
                    request_seq,
                    command,
                    Err("launch requires a \"program\" path".to_string()),
                );
                return;
            };
            let stop_on_entry = args
                .get("stopOnEntry")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match compile_file(&program) {
                Ok(cm) => {
                    *module = Some(Arc::new(cm));
                    *debuggee = Some(spawn_debuggee(
                        module.as_ref().unwrap().clone(),
                        program.clone(),
                        stop_on_entry,
                        control.clone(),
                        cmd_rx.clone(),
                        event_tx,
                    ));
                    respond(writer, seq, request_seq, command, Ok(json!({})));
                }
                Err(e) => respond(writer, seq, request_seq, command, Err(e)),
            }
        }
        "setBreakpoints" => {
            let source_path = args
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let requested: Vec<u32> = args
                .get("breakpoints")
                .and_then(|b| b.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| b.get("line").and_then(|l| l.as_i64()))
                        .map(|l| l.max(1) as u32)
                        .collect()
                })
                .unwrap_or_default();
            let _ = source_path;
            let result: Vec<Json> = match module.as_ref() {
                Some(m) => {
                    let mut c = control.lock();
                    c.breakpoints.clear();
                    let mut result = Vec::new();
                    for (i, line) in requested.into_iter().enumerate() {
                        let id = i as i64 + 1;
                        match m.resolve_line(line) {
                            Some((pc, actual_line)) => {
                                c.breakpoints.push(ResolvedBreakpoint { pc });
                                result.push(
                                    json!({ "id": id, "verified": true, "line": actual_line }),
                                );
                            }
                            None => {
                                result.push(json!({ "id": id, "verified": false, "line": line }))
                            }
                        }
                    }
                    result
                }
                None => requested
                    .iter()
                    .enumerate()
                    .map(|(i, l)| json!({ "id": i as i64 + 1, "verified": false, "line": l }))
                    .collect(),
            };
            respond(
                writer,
                seq,
                request_seq,
                command,
                Ok(json!({ "breakpoints": result })),
            );
        }
        "configurationDone" => {
            let _ = cmd_tx.send(DebugCommand::Start);
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        "threads" => respond(
            writer,
            seq,
            request_seq,
            command,
            Ok(json!({ "threads": [ { "id": 1, "name": "main" } ] })),
        ),
        "continue" => {
            control.lock().step = Some(StepMode::Run);
            let _ = cmd_tx.send(DebugCommand::Resume);
            respond(
                writer,
                seq,
                request_seq,
                command,
                Ok(json!({ "allThreadsContinued": true })),
            );
        }
        "next" => {
            control.lock().step = Some(StepMode::StepOver);
            let _ = cmd_tx.send(DebugCommand::Resume);
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        "stepIn" => {
            control.lock().step = Some(StepMode::StepIn);
            let _ = cmd_tx.send(DebugCommand::Resume);
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        "stepOut" => {
            control.lock().step = Some(StepMode::StepOut);
            let _ = cmd_tx.send(DebugCommand::Resume);
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        "pause" => {
            control.lock().pause_requested = true;
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        // --- Time-travel rewind (Wave E3, dev/staging; gated on the durable
        // store). These operate on persisted entity state, not the live
        // standalone-VM debuggee. Extension arguments: `actorId` (u64),
        // `targetSequence` (u64, reverseContinue only).
        "reverseContinue" => {
            let actor_id = args.get("actorId").and_then(|v| v.as_u64());
            let target = args.get("targetSequence").and_then(|v| v.as_u64());
            let result = match (actor_id, target) {
                (Some(a), Some(t)) => rewind.rewind_to(a, t),
                _ => Err("reverseContinue requires \"actorId\" and \"targetSequence\"".to_string()),
            };
            respond(writer, seq, request_seq, command, result);
        }
        "stepBack" => {
            let result = match args.get("actorId").and_then(|v| v.as_u64()) {
                Some(a) => rewind.step_back(a),
                None => Err("stepBack requires \"actorId\"".to_string()),
            };
            respond(writer, seq, request_seq, command, result);
        }
        "nulangStepForward" => {
            let result = match args.get("actorId").and_then(|v| v.as_u64()) {
                Some(a) => rewind.step_forward(a),
                None => Err("nulangStepForward requires \"actorId\"".to_string()),
            };
            respond(writer, seq, request_seq, command, result);
        }
        "stackTrace" => match request_snapshot(cmd_tx) {
            Ok(snap) => {
                let frames: Vec<Json> = snap
                    .get("frames")
                    .and_then(|f| f.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|f| {
                        json!({
                            "id": f["id"],
                            "name": f["name"],
                            "line": f["line"],
                            "column": 1,
                            "source": { "path": f["source"] }
                        })
                    })
                    .collect();
                respond(
                    writer,
                    seq,
                    request_seq,
                    command,
                    Ok(json!({ "stackFrames": frames, "totalFrames": frames.len() })),
                );
            }
            Err(e) => respond(writer, seq, request_seq, command, Err(e)),
        },
        "scopes" => {
            let frame_id = args.get("frameId").and_then(|v| v.as_i64()).unwrap_or(0);
            let scopes = json!([
                { "name": "Locals", "variablesReference": frame_id * 2 + 1, "expensive": false },
                { "name": "Registers", "variablesReference": frame_id * 2 + 2, "expensive": false }
            ]);
            respond(
                writer,
                seq,
                request_seq,
                command,
                Ok(json!({ "scopes": scopes })),
            );
        }
        "variables" => {
            let vref = args
                .get("variablesReference")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // Odd variablesReference = Locals scope, even = Registers scope
            // (see the `scopes` handler's encoding frameId*2+{1,2}).
            let is_registers = vref % 2 == 0;
            let frame_id = ((vref - 1) / 2) as usize;
            match request_snapshot(cmd_tx) {
                Ok(snap) => {
                    let frame = snap
                        .get("frames")
                        .and_then(|f| f.as_array())
                        .and_then(|arr| arr.get(frame_id));
                    let list = if is_registers {
                        frame.and_then(|f| f.get("registers"))
                    } else {
                        frame.and_then(|f| f.get("locals"))
                    };
                    let vars: Vec<Json> = list
                        .and_then(|l| l.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|kv| {
                                    json!({
                                        "name": kv[0],
                                        "value": kv[1],
                                        "variablesReference": 0
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    respond(
                        writer,
                        seq,
                        request_seq,
                        command,
                        Ok(json!({ "variables": vars })),
                    );
                }
                Err(e) => respond(writer, seq, request_seq, command, Err(e)),
            }
        }
        "evaluate" => {
            let expr = args
                .get("expression")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let frame_id = args.get("frameId").and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            match request_snapshot(cmd_tx) {
                Ok(snap) => {
                    let found = snap
                        .get("frames")
                        .and_then(|f| f.as_array())
                        .and_then(|arr| arr.get(frame_id))
                        .and_then(|f| f.get("locals"))
                        .and_then(|l| l.as_array())
                        .and_then(|arr| {
                            arr.iter().find(|kv| kv[0].as_str() == Some(expr.as_str()))
                        });
                    match found {
                        Some(kv) => respond(
                            writer,
                            seq,
                            request_seq,
                            command,
                            Ok(json!({ "result": kv[1] })),
                        ),
                        None => {
                            if let Ok(v) = expr.parse::<i64>() {
                                respond(
                                    writer,
                                    seq,
                                    request_seq,
                                    command,
                                    Ok(json!({ "result": format!("{}", v) })),
                                );
                            } else if let Ok(v) = expr.parse::<f64>() {
                                respond(
                                    writer,
                                    seq,
                                    request_seq,
                                    command,
                                    Ok(json!({ "result": format!("{}", v) })),
                                );
                            } else if expr == "true" || expr == "false" {
                                respond(
                                    writer,
                                    seq,
                                    request_seq,
                                    command,
                                    Ok(json!({ "result": expr })),
                                );
                            } else {
                                respond(
                                    writer,
                                    seq,
                                    request_seq,
                                    command,
                                    Err(format!("no local variable or literal named '{}'", expr)),
                                );
                            }
                        }
                    }
                }
                Err(e) => respond(writer, seq, request_seq, command, Err(e)),
            }
        }
        "terminate" | "disconnect" => {
            let _ = cmd_tx.send(DebugCommand::Terminate);
            if let Some(h) = debuggee.take() {
                let _ = h.join();
            }
            respond(writer, seq, request_seq, command, Ok(json!({})));
        }
        "setExceptionBreakpoints" => respond(
            writer,
            seq,
            request_seq,
            command,
            Ok(json!({ "breakpoints": [] })),
        ),
        _ => respond(
            writer,
            seq,
            request_seq,
            command,
            Err(format!("unsupported command '{}'", command)),
        ),
    }
}

/// Compile a `.nula` file to a `CodeModule`, resolving the display path for
/// diagnostics.
fn compile_file(path: &str) -> Result<CodeModule, String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;
    let name = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".to_string());
    compile_source(&source, Some(path), &name).map_err(|e| format!("{}", e))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Run the DAP server on the process stdin/stdout.
pub fn run_dap_server() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_dap_server_io(std::io::BufReader::new(stdin), BufWriter::new(stdout));
}

/// Drive the DAP server over arbitrary `Read`/`Write` streams (used by tests
/// to exercise the adapter in-process without a real editor). Returns when
/// `reader` reaches EOF.
pub fn run_dap_server_io<R, W>(reader: R, writer: W)
where
    R: BufRead + Send + 'static,
    W: Write,
{
    // Store resolution mirrors the CLI: NULANG_STORE_PATH, else
    // .nulang/store/ when it exists. Tests use `run_dap_server_io_with_store`.
    run_dap_server_io_with_store(reader, writer, rewind::resolve_store_dir())
}

/// Like [`run_dap_server_io`] but with an explicit durable store directory
/// override (`Some` enables time-travel rewind against that store; `None`
/// disables rewind regardless of the environment).
pub fn run_dap_server_io_with_store<R, W>(
    reader: R,
    writer: W,
    store_dir: Option<std::path::PathBuf>,
) where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (msg_tx, msg_rx): (Sender<Json>, Receiver<Json>) = crossbeam::channel::unbounded();
    let reader_thread = std::thread::spawn(move || {
        let mut r = reader;
        while let Some(msg) = read_message(&mut r) {
            if msg_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let (event_tx, event_rx): (Sender<DebugEvent>, Receiver<DebugEvent>) =
        crossbeam::channel::unbounded();
    let (cmd_tx, cmd_rx): (Sender<DebugCommand>, Receiver<DebugCommand>) =
        crossbeam::channel::unbounded();

    let control = Arc::new(Mutex::new(ControlState::new()));
    let mut rewind = RewindSession::new(store_dir);
    let mut module: Option<Arc<CodeModule>> = None;
    let mut debuggee: Option<std::thread::JoinHandle<()>> = None;
    let mut seq: i64 = 0;
    let mut writer = writer;

    loop {
        // Drain pending debuggee events promptly so `stopped` reaches the
        // client without waiting for the next request.
        loop {
            match event_rx.try_recv() {
                Ok(ev) => write_event(&mut writer, &mut seq, ev),
                Err(_) => break,
            }
        }
        crossbeam::select! {
            recv(event_rx) -> ev => {
                if let Ok(ev) = ev {
                    write_event(&mut writer, &mut seq, ev);
                }
            }
            recv(msg_rx) -> m => {
                match m {
                    Ok(msg) => {
                        let request_seq = msg.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
                        let command = msg.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args = msg.get("arguments").cloned().unwrap_or(Json::Null);
                        handle_request(&mut writer, &mut seq, request_seq, &command, &args, &control, &mut debuggee, &mut module, &cmd_tx, &cmd_rx, &event_tx, &mut rewind);
                    }
                    Err(_) => {
                        // Client disconnected. Drain debuggee events until it
                        // settles (finishes, or pauses with no further output
                        // for the quiescence window), then shut down.
                        loop {
                            // Drain everything already queued first: the
                            // debuggee may have sent a burst (e.g. Exited +
                            // Terminated) and finished, so a naive
                            // `is_finished()` short-circuit would drop the
                            // trailing events still in the queue.
                            while let Ok(ev) = event_rx.try_recv() {
                                write_event(&mut writer, &mut seq, ev);
                            }
                            if debuggee.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                                break;
                            }
                            match event_rx.recv_timeout(std::time::Duration::from_millis(250)) {
                                Ok(ev) => write_event(&mut writer, &mut seq, ev),
                                Err(_) => break,
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    let _ = reader_thread.join();
    // Terminate any still-running debuggee on shutdown.
    let _ = cmd_tx.send(DebugCommand::Terminate);
    if let Some(h) = debuggee.take() {
        let _ = h.join();
    }
}

// Re-exports used by tests.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn frame(message: &Json) -> String {
        let body = serde_json::to_string(message).unwrap();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
    }

    fn send(sink: &mut Vec<u8>, msg: &Json) {
        sink.extend_from_slice(frame(msg).as_bytes());
    }

    fn parse_output(output: &[u8]) -> Vec<Json> {
        let mut msgs = Vec::new();
        let mut cursor = std::io::Cursor::new(output.to_vec());
        while let Some(m) = read_message(&mut cursor) {
            msgs.push(m);
        }
        msgs
    }

    fn req(seq: i64, command: &str, args: Json) -> Json {
        json!({ "seq": seq, "type": "request", "command": command, "arguments": args })
    }

    /// Write `source` to a fresh unique temp file and return its path.
    fn write_prog(source: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("nulang-dap-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("prog.nula");
        std::fs::write(&path, source).unwrap();
        path
    }

    /// Drive a scripted DAP session against `source` and return the framed
    /// output (responses + events) as parsed JSON messages.
    fn run_session(source: &str, script: &[Json]) -> Vec<Json> {
        let path = write_prog(source);
        let prog = path.to_string_lossy().to_string();
        // Substitute the real temp path for the "<prog>" placeholder in any
        // launch / setBreakpoints source references.
        let script: Vec<Json> = script
            .iter()
            .map(|m| {
                let mut m = m.clone();
                let cmd = m
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if cmd == "launch" {
                    if let Some(prog_arg) = m.pointer_mut("/arguments/program") {
                        *prog_arg = Json::String(prog.clone());
                    }
                } else if cmd == "setBreakpoints" {
                    if let Some(p) = m.pointer_mut("/arguments/source/path") {
                        *p = Json::String(prog.clone());
                    }
                }
                m
            })
            .collect();
        let mut input = Vec::new();
        for m in &script {
            send(&mut input, m);
        }
        let mut output = Vec::new();
        let reader = std::io::Cursor::new(input);
        run_dap_server_io(std::io::BufReader::new(reader), &mut output);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        parse_output(&output)
    }

    #[test]
    fn test_initialize_and_capabilities() {
        let msgs = run_session("let x = 1 in x", &[req(1, "initialize", json!({}))]);
        assert_eq!(msgs.len(), 1, "expected 1 response, got {:#?}", msgs);
        assert_eq!(msgs[0]["type"], "response");
        assert_eq!(msgs[0]["command"], "initialize");
        assert_eq!(msgs[0]["success"], true);
        assert_eq!(msgs[0]["body"]["supportsEvaluate"], true);
    }

    #[test]
    fn test_breakpoint_hit_stops() {
        // A function call with statements on distinct lines; breakpoint on
        // the statement at line 3.
        let source = "let a = 1 in {\n  let b = a + 1 in {\n    let c = b + 2 in c\n  }\n}";
        let script = vec![
            req(1, "initialize", json!({})),
            req(2, "launch", json!({ "program": "<prog>" })),
            req(
                3,
                "setBreakpoints",
                json!({ "source": { "path": "<prog>" }, "breakpoints": [ { "line": 3 } ] }),
            ),
            req(4, "configurationDone", json!({})),
        ];
        let msgs = run_session(source, &script);

        let stopped = msgs.iter().find(|m| m["event"] == "stopped");
        assert!(
            stopped.is_some(),
            "expected a stopped event, got {:#?}",
            msgs
        );
        assert_eq!(stopped.unwrap()["body"]["reason"], "breakpoint");
    }

    #[test]
    fn test_continue_reaches_exit() {
        let source = "let a = 1 in a + 1";
        let script = vec![
            req(1, "initialize", json!({})),
            req(2, "launch", json!({ "program": "<prog>" })),
            req(
                3,
                "setBreakpoints",
                json!({ "source": { "path": "<prog>" }, "breakpoints": [] }),
            ),
            req(4, "configurationDone", json!({})),
        ];
        let msgs = run_session(source, &script);

        // No breakpoints set -> the program runs to completion.
        assert!(
            msgs.iter().any(|m| m["event"] == "exited"),
            "expected exited, got {:#?}",
            msgs
        );
        assert!(
            msgs.iter().any(|m| m["event"] == "terminated"),
            "expected terminated, got {:#?}",
            msgs
        );
    }

    #[test]
    fn test_state_queries_while_paused() {
        // Breakpoint on line 2 (`let b = 2`): when paused there, local `a` is 1.
        let source = "let a = 1 in {\n  let b = 2 in {\n    let c = a + b in c\n  }\n}";
        let script = vec![
            req(1, "initialize", json!({})),
            req(2, "launch", json!({ "program": "<prog>" })),
            req(
                3,
                "setBreakpoints",
                json!({ "source": { "path": "<prog>" }, "breakpoints": [ { "line": 2 } ] }),
            ),
            req(4, "configurationDone", json!({})),
            req(5, "stackTrace", json!({ "threadId": 1 })),
            req(6, "scopes", json!({ "frameId": 0 })),
            req(7, "variables", json!({ "variablesReference": 1 })), // Locals scope of frame 0
            req(8, "variables", json!({ "variablesReference": 2 })), // Registers scope of frame 0
            req(9, "evaluate", json!({ "expression": "a", "frameId": 0 })),
        ];
        let msgs = run_session(source, &script);

        let stopped = msgs.iter().find(|m| m["event"] == "stopped");
        assert!(
            stopped.is_some(),
            "expected a stopped event, got {:#?}",
            msgs
        );
        assert_eq!(stopped.unwrap()["body"]["reason"], "breakpoint");

        // stackTrace: frame carries a real source path (the program file).
        let st = msgs
            .iter()
            .find(|m| m["command"] == "stackTrace" && m["type"] == "response");
        let st = st.expect("expected a stackTrace response");
        assert!(
            st["success"].as_bool().unwrap_or(false),
            "stackTrace failed: {:#?}",
            st
        );
        let frame = &st["body"]["stackFrames"][0];
        assert!(
            frame["source"]["path"].is_string(),
            "source.path must be a path, got {:#?}",
            frame["source"]
        );
        assert!(!frame["name"].as_str().unwrap_or("").is_empty());

        // scopes: Locals + Registers.
        let scopes = msgs
            .iter()
            .find(|m| m["command"] == "scopes" && m["type"] == "response");
        let scopes = scopes.expect("expected a scopes response");
        let names: Vec<_> = scopes["body"]["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            names.contains(&"Locals") && names.contains(&"Registers"),
            "got scopes {:#?}",
            names
        );

        // variables (Locals): the paused frame exposes local `a` = 1.
        let vars = msgs.iter().find(|m| {
            m["command"] == "variables" && m["type"] == "response" && m["request_seq"] == 7
        });
        let vars = vars.expect("expected a Locals variables response");
        let a = vars["body"]["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "a");
        assert!(a.is_some(), "local 'a' not found in {:#?}", vars);
        assert_eq!(a.unwrap()["value"], "1");

        // variables (Registers): non-nil registers are reported (r16 = LOCAL_BASE for `a`).
        let regs = msgs.iter().find(|m| {
            m["command"] == "variables" && m["type"] == "response" && m["request_seq"] == 8
        });
        let regs = regs.expect("expected a Registers variables response");
        assert!(
            regs["body"]["variables"]
                .as_array()
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            "expected non-empty registers, got {:#?}",
            regs
        );

        // evaluate "a" resolves to 1.
        let ev = msgs
            .iter()
            .find(|m| m["command"] == "evaluate" && m["type"] == "response");
        let ev = ev.expect("expected an evaluate response");
        assert_eq!(ev["body"]["result"], "1");
    }

    // ---------------------------------------------------------------------
    // Time-travel rewind (Wave E3)
    // ---------------------------------------------------------------------

    /// Like `run_session`, but with an explicit durable-store override:
    /// `Some(dir)` enables rewind against `dir`, `None` disables it.
    fn run_session_with_store(
        source: &str,
        script: &[Json],
        store_dir: Option<std::path::PathBuf>,
    ) -> Vec<Json> {
        let path = write_prog(source);
        let prog = path.to_string_lossy().to_string();
        let script: Vec<Json> = script
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if m.get("command").and_then(|c| c.as_str()) == Some("launch") {
                    if let Some(p) = m.pointer_mut("/arguments/program") {
                        *p = Json::String(prog.clone());
                    }
                }
                m
            })
            .collect();
        let mut input = Vec::new();
        for m in &script {
            send(&mut input, m);
        }
        let mut output = Vec::new();
        let reader = std::io::Cursor::new(input);
        run_dap_server_io_with_store(std::io::BufReader::new(reader), &mut output, store_dir);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        parse_output(&output)
    }

    /// Create a temp store dir holding entity 7 with a snapshot at seq 2
    /// (count=10) and events 3..=5 (count=11,12,13); returns the dir.
    fn write_store() -> std::path::PathBuf {
        use crate::runtime::{
            ActorSnapshot, EventEntry, JournalEntry, JsonFileStore, PersistedValue,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("nulang-dap-store-{}-{}", std::process::id(), n));
        let mut store = JsonFileStore::new(&dir).unwrap();
        let mut snap = ActorSnapshot::default();
        snap.actor_id = 7;
        snap.sequence = 2;
        snap.state
            .insert("count".to_string(), PersistedValue::Int(10));
        store.save_snapshot(snap).unwrap();
        for (s, v) in [(3u64, 11i64), (4, 12), (5, 13)] {
            store
                .append_event(
                    7,
                    EventEntry {
                        sequence: s,
                        field_name: "count".to_string(),
                        event_name: "Incremented".to_string(),
                        args: vec![],
                        value: PersistedValue::Int(v),
                    },
                )
                .unwrap();
            store
                .append_journal(
                    7,
                    JournalEntry {
                        sequence: s,
                        behavior_id: 0,
                        payload: vec![],
                    },
                )
                .unwrap();
        }
        dir
    }

    #[test]
    fn test_rewind_gated_off_without_store() {
        // With no durable store, rewind is not advertised and requests fail.
        let script = vec![
            req(1, "initialize", json!({})),
            req(
                2,
                "reverseContinue",
                json!({ "actorId": 7, "targetSequence": 3 }),
            ),
        ];
        let msgs = run_session_with_store("let x = 1 in x", &script, None);
        let init = &msgs[0];
        assert_eq!(init["body"]["supportsReverseContinueRequest"], false);
        let rc = msgs
            .iter()
            .find(|m| m["command"] == "reverseContinue")
            .expect("expected a reverseContinue response");
        assert_eq!(rc["success"], false, "expected failure, got {:#?}", rc);
    }

    #[test]
    fn test_reverse_continue_step_back_and_forward() {
        let dir = write_store();
        let script = vec![
            req(1, "initialize", json!({})),
            // Rewind entity 7 to message #3 -> count = 11.
            req(
                2,
                "reverseContinue",
                json!({ "actorId": 7, "targetSequence": 3 }),
            ),
            // Step back to #2 -> snapshot value count = 10.
            req(3, "stepBack", json!({ "actorId": 7 })),
            // Step forward twice: #3 (11), then #4 (12).
            req(4, "nulangStepForward", json!({ "actorId": 7 })),
            req(5, "nulangStepForward", json!({ "actorId": 7 })),
        ];
        let msgs = run_session_with_store("let x = 1 in x", &script, Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);

        let init = &msgs[0];
        assert_eq!(init["body"]["supportsReverseContinueRequest"], true);
        assert_eq!(init["body"]["supportsStepBack"], true);

        let resp = |seq: i64, cmd: &str| -> &Json {
            msgs.iter()
                .find(|m| m["command"] == cmd && m["type"] == "response" && m["request_seq"] == seq)
                .unwrap_or_else(|| {
                    panic!("expected {} response for seq {}, got {:#?}", cmd, seq, msgs)
                })
        };
        let rc = resp(2, "reverseContinue");
        assert_eq!(rc["success"], true, "got {:#?}", rc);
        assert_eq!(rc["body"]["sequence"], 3);
        assert_eq!(rc["body"]["latestSequence"], 5);
        assert_eq!(rc["body"]["state"]["count"], 11);
        assert_eq!(rc["body"]["journal"].as_array().unwrap().len(), 1);

        let sb = resp(3, "stepBack");
        assert_eq!(sb["body"]["sequence"], 2);
        assert_eq!(sb["body"]["state"]["count"], 10);

        let f1 = resp(4, "nulangStepForward");
        assert_eq!(f1["body"]["sequence"], 3);
        assert_eq!(f1["body"]["state"]["count"], 11);
        let f2 = resp(5, "nulangStepForward");
        assert_eq!(f2["body"]["sequence"], 4);
        assert_eq!(f2["body"]["state"]["count"], 12);
    }
}
