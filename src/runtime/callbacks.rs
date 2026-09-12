//! VM callback bridges — connects the bytecode VM to a real `Runtime`.
//!
//! Three `ActorVmCallbacks`/`DistributedVmCallbacks` implementations, split
//! out of `runtime/mod.rs` (2026-08-02) since they're a cohesive, mostly
//! self-contained layer (built-in effect dispatch, heap alloc routing) with
//! no state of their own beyond a `Runtime` handle:
//! - [`RuntimeVmCallbacks`] — `Rc<RefCell<Runtime>>` handle, used by the
//!   top-level VM (outside any scheduler-driven behavior): `main.rs`,
//!   integration tests, `runtime/tests.rs`.
//! - `BytecodeRuntimeCallbacks` — raw `*mut Runtime` handle, used when the
//!   runtime drives a behavior's bytecode from inside the scheduler
//!   (`run_bytecode_at_offset` and friends in `runtime/mod.rs`, plus
//!   `runtime/workflow.rs`).
//! - `BytecodeDistributedCallbacks` — same raw-pointer pattern, for the
//!   `DistributedVmCallbacks` trait (`RSend`/`RAsk`/`Migrate`/`RSpawn`/
//!   `Gossip` opcodes).

#[cfg(feature = "ai-runtime")]
use super::agent;
use super::cluster::NodeId;
use super::distributed::{send_distributed, spawn_on_node, ActorAddress};
use super::http_server::HttpServerState;
use super::Runtime;
use crate::runtime::heap::{ActorHeap, TypeTag as HeapTypeTag};
#[cfg(feature = "ai-runtime")]
use nulang_ai::{LlmMessage, LlmRequest};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// Spawn using authority metadata attached to the exact executing bytecode PC.
/// Any malformed metadata or parent escalation fails closed before a child is
/// created or enqueued.
pub(crate) fn spawn_with_site_authority(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    spawn_pc: usize,
    behavior_idx: usize,
    init: Vec<(String, crate::vm::Value)>,
) -> crate::vm::Value {
    let requested = match crate::authority_runtime::spawn_authority_manifest(module, spawn_pc) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn with invalid authority metadata"
            );
            return crate::vm::Value::nil();
        }
    };

    match super::spawn::spawn_from_module_with_authority(rt, module, behavior_idx, init, &requested)
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn whose authority is not delegated by the parent"
            );
            crate::vm::Value::nil()
        }
    }
}

/// Resolve the exact typed authority required before an actor-backed host
/// effect may cross the runtime boundary. `None` means the effect is not an
/// external-authority operation handled by this gate. Invalid resource
/// descriptions are errors rather than permissive fallbacks.
fn required_host_authority(
    effect_name: &str,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Result<Option<crate::authority::AuthorityGrant>, String> {
    use crate::authority::AuthorityGrant;

    let string_arg = |idx: usize| -> Result<String, String> {
        let value = regs
            .get(idx)
            .copied()
            .ok_or_else(|| format!("missing host-effect argument {idx}"))?;
        let value = crate::vm::resolve_value_string(constants, value);
        if value.is_empty() {
            return Err(format!("host-effect argument {idx} must be non-empty"));
        }
        Ok(value)
    };
    let other = |namespace: &str, operation: &str, argument: Option<String>| {
        AuthorityGrant::Other {
            namespace: namespace.to_string(),
            operation: operation.to_string(),
            argument,
        }
    };

    let grant = match (effect_name, op_name) {
        ("FS", Some("read" | "exists")) => AuthorityGrant::FsRead {
            path: string_arg(0)?,
        },
        ("FS", Some("write" | "append")) => AuthorityGrant::FsWrite {
            path: string_arg(0)?,
        },
        ("Web", Some("serve_static")) => AuthorityGrant::FsRead {
            path: string_arg(0)?,
        },
        ("Realtime", Some("broadcast")) => {
            other("Realtime", "Broadcast", Some(string_arg(0)?))
        },
        ("Env", Some("get")) => AuthorityGrant::EnvRead {
            name: string_arg(0)?,
        },
        ("Secret", Some("read" | "get")) => AuthorityGrant::SecretRead {
            name: string_arg(0)?,
        },
        ("Http", Some("get" | "post")) => http_outbound_authority(&string_arg(0)?)?,
        ("Http", Some("serve")) => {
            let port = regs
                .first()
                .and_then(|v| v.as_int())
                .ok_or_else(|| "Http.serve requires an integer port".to_string())?;
            if !(0..=u16::MAX as i64).contains(&port) {
                return Err("Http.serve port is outside u16 range".to_string());
            }
            other("Net", "Listen", Some(port.to_string()))
        }
        ("Process", Some("run")) => other("Process", "Run", Some(string_arg(0)?)),
        ("System", Some("arg")) => {
            let index = regs
                .first()
                .and_then(|v| v.as_int())
                .ok_or_else(|| "System.arg requires an integer index".to_string())?;
            if index < 0 {
                return Err("System.arg index must be non-negative".to_string());
            }
            other("System", "Arg", Some(index.to_string()))
        }
        ("DB", Some("query")) => other("DB", "Query", None),
        ("Python", Some(op)) if !op.is_empty() => other("Python", op, None),
        ("Provider", Some("ask")) => other("Provider", "Ask", Some(string_arg(0)?)),
        ("Inference" | "LLM", Some("ask")) => other("Inference", "Ask", None),
        _ => return Ok(None),
    };
    Ok(Some(grant))
}

/// Parse an outbound HTTP URL into the exact TCP authority it needs. This is
/// deliberately small and fail-closed: only lowercase `http://` and
/// `https://` URLs are accepted, userinfo is rejected, and an ambiguous or
/// malformed authority never turns into a broader grant.
fn http_outbound_authority(url: &str) -> Result<crate::authority::AuthorityGrant, String> {
    let (rest, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443u16)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80u16)
    } else {
        return Err("outbound HTTP authority requires http:// or https://".to_string());
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or_else(|| "outbound HTTP URL has no authority".to_string())?;
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return Err("outbound HTTP URL has invalid authority".to_string());
    }

    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| "unterminated bracketed HTTP host".to_string())?;
        let host = &bracketed[..close];
        if host.is_empty() {
            return Err("outbound HTTP host is empty".to_string());
        }
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            let raw = suffix
                .strip_prefix(':')
                .ok_or_else(|| "invalid bracketed HTTP authority suffix".to_string())?;
            parse_http_port(raw)?
        };
        (format!("[{host}]"), port)
    } else {
        if authority.matches(':').count() > 1 {
            return Err("IPv6 HTTP hosts must be bracketed".to_string());
        }
        match authority.rsplit_once(':') {
            Some((host, raw_port)) => {
                if host.is_empty() {
                    return Err("outbound HTTP host is empty".to_string());
                }
                (host.to_ascii_lowercase(), parse_http_port(raw_port)?)
            }
            None => (authority.to_ascii_lowercase(), default_port),
        }
    };

    if host.is_empty() {
        return Err("outbound HTTP host is empty".to_string());
    }
    Ok(crate::authority::AuthorityGrant::NetTcpOut { host, port })
}

fn parse_http_port(raw: &str) -> Result<u16, String> {
    let port = raw
        .parse::<u16>()
        .map_err(|_| "outbound HTTP port must be a valid u16".to_string())?;
    if port == 0 {
        return Err("outbound HTTP port must be non-zero".to_string());
    }
    Ok(port)
}

/// Enforce actor authority at the last common callback boundary before a host
/// operation executes. Actor-free runtime/top-level execution intentionally
/// retains its existing ambient behavior; #165 scopes this migration to
/// actor-backed authority. A missing actor or malformed manifest fails closed.
fn authorize_actor_host_effect(
    rt: &Runtime,
    actor_id: Option<u64>,
    effect_name: &str,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Result<(), String> {
    let Some(actor_id) = actor_id else {
        return Ok(());
    };
    let Some(grant) = required_host_authority(effect_name, op_name, constants, regs)? else {
        return Ok(());
    };
    let actor = rt
        .actors
        .get(&actor_id)
        .ok_or_else(|| format!("authority source actor {actor_id} is missing"))?;
    actor.require_authority(&grant).map_err(|error| error.to_string())
}

/// Enforce an exact actor-backed foreign-function authority grant.
///
/// The grant argument includes both library and symbol so permission to call
/// one imported function cannot authorize another symbol from the same
/// dynamic library. Actor-free execution keeps the legacy ambient contract.
pub(crate) fn authorize_actor_ffi(
    rt: &Runtime,
    actor_id: Option<u64>,
    library: &str,
    symbol: &str,
) -> Result<(), String> {
    let Some(actor_id) = actor_id else {
        return Ok(());
    };
    if library.is_empty() || symbol.is_empty() {
        return Err("FFI library and symbol must be non-empty".to_string());
    }
    let actor = rt
        .actors
        .get(&actor_id)
        .ok_or_else(|| format!("authority source actor {actor_id} is missing"))?;
    let grant = crate::authority::AuthorityGrant::Other {
        namespace: "FFI".to_string(),
        operation: "Call".to_string(),
        argument: Some(format!("{library}::{symbol}")),
    };
    actor.require_authority(&grant).map_err(|error| error.to_string())
}

/// Shared Web effect host implementation for all runtime callback types.
/// Mirrors the standalone VM dispatch in `src/vm.rs`.
pub(crate) fn perform_web_builtin(
    callbacks: &mut dyn crate::vm::ActorVmCallbacks,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Option<crate::vm::Value> {
    fn read_array(
        _callbacks: &dyn crate::vm::ActorVmCallbacks,
        value: crate::vm::Value,
    ) -> Vec<crate::vm::Value> {
        if let Some(ptr) = value.as_ptr() {
            unsafe {
                let header = &*ActorHeap::header_of(ptr);
                if header.type_tag == HeapTypeTag::Array || header.type_tag == HeapTypeTag::Tuple {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let len = payload_size / std::mem::size_of::<crate::vm::Value>();
                    if len > 0 {
                        return std::slice::from_raw_parts(ptr as *const crate::vm::Value, len)
                            .to_vec();
                    }
                }
            }
        }
        Vec::new()
    }
    fn resolve(constants: &[crate::bytecode::Constant], value: crate::vm::Value) -> String {
        crate::vm::resolve_value_string(constants, value)
    }
    fn html_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\"', "&quot;")
            .replace('\'', "&#x27;")
    }

    fn alloc_tuple(
        callbacks: &mut dyn crate::vm::ActorVmCallbacks,
        a: crate::vm::Value,
        b: crate::vm::Value,
    ) -> crate::vm::Value {
        let payload_size = 2 * std::mem::size_of::<crate::vm::Value>();
        match callbacks.alloc(payload_size, HeapTypeTag::Tuple) {
            Some(ptr) => {
                // SAFETY: alloc returned a fresh payload of exactly the
                // requested size; writing two Values fits exactly.
                unsafe {
                    let slots = ptr as *mut crate::vm::Value;
                    slots.write(a);
                    slots.add(1).write(b);
                }
                crate::vm::Value::ptr(ptr)
            }
            None => crate::vm::Value::nil(),
        }
    }
    fn alloc_array(
        callbacks: &mut dyn crate::vm::ActorVmCallbacks,
        items: Vec<crate::vm::Value>,
    ) -> crate::vm::Value {
        if items.is_empty() {
            return crate::vm::Value::nil();
        }
        let payload_size = items.len() * std::mem::size_of::<crate::vm::Value>();
        match callbacks.alloc(payload_size, HeapTypeTag::Array) {
            Some(ptr) => {
                // SAFETY: alloc returned a fresh payload of exactly the
                // requested size; writing all Values fits exactly.
                unsafe {
                    let slots = ptr as *mut crate::vm::Value;
                    for (i, item) in items.iter().enumerate() {
                        slots.add(i).write(*item);
                    }
                }
                crate::vm::Value::ptr(ptr)
            }
            None => crate::vm::Value::nil(),
        }
    }
    match op_name {
        Some("html") => {
            let tag = resolve(constants, *regs.first()?);
            let attrs = read_array(callbacks, *regs.get(1)?);
            let children = read_array(callbacks, *regs.get(2)?);
            let mut out = String::new();
            out.push('<');
            out.push_str(&tag);
            for attr in &attrs {
                let slots = read_array(callbacks, *attr);
                if slots.len() >= 2 {
                    let name = resolve(constants, slots[0]);
                    let value = resolve(constants, slots[1]);
                    out.push(' ');
                    out.push_str(&name);
                    out.push_str("=\"");
                    out.push_str(&value);
                    out.push('\"');
                }
            }
            out.push('>');
            for child in &children {
                out.push_str(&resolve(constants, *child));
            }
            out.push_str("</");
            out.push_str(&tag);
            out.push('>');
            Some(callbacks.alloc_string(&out))
        }
        Some("text") => {
            let s = resolve(constants, *regs.first()?);
            Some(callbacks.alloc_string(&html_escape(&s)))
        }
        Some("raw") => {
            let s = resolve(constants, *regs.first()?);
            Some(callbacks.alloc_string(&s))
        }
        Some("route") => Some(crate::vm::Value::unit()),
        Some("redirect") => {
            let url = resolve(constants, *regs.first()?);
            let out = format!(
                "<html><head><meta http-equiv=\"refresh\" content=\"0;url={}\"></head></html>",
                url
            );
            Some(callbacks.alloc_string(&out))
        }
        Some("serve_static") => {
            let path = resolve(constants, *regs.first()?);
            match std::fs::read_to_string(&path) {
                Ok(content) => Some(callbacks.alloc_string(&content)),
                Err(_) => Some(crate::vm::Value::nil()),
            }
        }
        Some("param") => {
            let name = resolve(constants, *regs.first()?);
            crate::runtime::http_server::current_request_param(&name)
                .map(|s| callbacks.alloc_string(&s))
                .or(Some(crate::vm::Value::nil()))
        }
        Some("header") => {
            let name = resolve(constants, *regs.first()?);
            crate::runtime::http_server::current_request_header(&name)
                .map(|s| callbacks.alloc_string(&s))
                .or(Some(crate::vm::Value::nil()))
        }
        Some("cookie") => {
            let name = resolve(constants, *regs.first()?);
            crate::runtime::http_server::current_request_cookie(&name)
                .map(|s| callbacks.alloc_string(&s))
                .or(Some(crate::vm::Value::nil()))
        }
        Some("read_body") => crate::runtime::http_server::current_request_body()
            .map(|b| callbacks.alloc_string(&String::from_utf8_lossy(&b)))
            .or(Some(crate::vm::Value::nil())),
        Some("method") => crate::runtime::http_server::current_request_method()
            .map(|s| callbacks.alloc_string(&s))
            .or(Some(crate::vm::Value::nil())),
        Some("form") => {
            let pairs = crate::runtime::http_server::current_request_body()
                .map(|b| crate::runtime::http_server::parse_form_urlencoded(&b))
                .unwrap_or_default();
            let mut tuples: Vec<crate::vm::Value> = Vec::new();
            for (k, v) in pairs {
                let k_val = callbacks.alloc_string(&k);
                let v_val = callbacks.alloc_string(&v);
                tuples.push(alloc_tuple(callbacks, k_val, v_val));
            }
            Some(alloc_array(callbacks, tuples))
        }
        Some("form_value") => {
            let name = resolve(constants, *regs.first()?);
            crate::runtime::http_server::form_value(&name)
                .map(|s| callbacks.alloc_string(&s))
                .or(Some(crate::vm::Value::nil()))
        }
        Some("kv_get") => {
            let key = resolve(constants, *regs.first()?);
            crate::runtime::http_server::kv_get(&key)
                .map(|s| callbacks.alloc_string(&s))
                .or(Some(crate::vm::Value::nil()))
        }
        Some("kv_set") => {
            let key = resolve(constants, *regs.first()?);
            let value = resolve(constants, *regs.get(1)?);
            crate::runtime::http_server::kv_set(&key, &value);
            Some(crate::vm::Value::unit())
        }
        Some("kv_delete") => {
            let key = resolve(constants, *regs.first()?);
            crate::runtime::http_server::kv_delete(&key);
            Some(crate::vm::Value::unit())
        }
        Some("kv_all") => {
            let pairs = crate::runtime::http_server::kv_all();
            let mut tuples: Vec<crate::vm::Value> = Vec::new();
            for (k, v) in pairs {
                let k_val = callbacks.alloc_string(&k);
                let v_val = callbacks.alloc_string(&v);
                tuples.push(alloc_tuple(callbacks, k_val, v_val));
            }
            Some(alloc_array(callbacks, tuples))
        }
        Some("set_cookie") => {
            let name = resolve(constants, *regs.first()?);
            let value = resolve(constants, *regs.get(1)?);
            crate::runtime::http_server::set_cookie(&name, &value);
            Some(crate::vm::Value::unit())
        }
        Some("clear_cookie") => {
            let name = resolve(constants, *regs.first()?);
            crate::runtime::http_server::clear_cookie(&name);
            Some(crate::vm::Value::unit())
        }
        _ => None,
    }
}

/// Realtime built-in effect operations for the web framework.
///
/// Mirrors the standalone VM dispatch in `src/vm.rs`.
pub(crate) fn perform_realtime_builtin(
    _callbacks: &mut dyn crate::vm::ActorVmCallbacks,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Option<crate::vm::Value> {
    fn resolve(constants: &[crate::bytecode::Constant], value: crate::vm::Value) -> String {
        crate::vm::resolve_value_string(constants, value)
    }
    match op_name {
        Some("broadcast") => {
            let room = resolve(constants, *regs.first()?);
            let message = resolve(constants, *regs.get(1)?);
            crate::runtime::http_server::realtime_broadcast(&room, &message);
            Some(crate::vm::Value::unit())
        }
        _ => None,
    }
}

/// Bridges the standalone VM to a real `Runtime`.
///
/// Used in tests and in any context where bytecode should create real actors
/// and allocate on the current actor's heap.
pub struct RuntimeVmCallbacks {
    runtime: Rc<RefCell<Runtime>>,
}

impl RuntimeVmCallbacks {
    pub fn new(runtime: Rc<RefCell<Runtime>>) -> Self {
        RuntimeVmCallbacks { runtime }
    }

    /// Allocate a fresh heap string via `self.alloc` (the current actor's
    /// heap, or `Runtime::main_heap` outside any actor context) and copy
    /// `s`'s bytes into it, null-terminated. Mirrors `VM::allocate_string`,
    /// but through THIS callback's own (now-correct) allocator rather than
    /// reaching into `Runtime.vm` — a separate, lazily-created VM instance
    /// used only to run actor bytecode, whose heap is not the heap this
    /// callback's caller (e.g. `main()`'s own top-level VM) can read back
    /// from. Builtin effects that produce a NEW string (`Int.to_string`,
    /// `Float.to_string`, JSON/LLM results, ...) must allocate through this
    /// helper, not `rt.vm.allocate_string`.
    fn alloc_string(&mut self, s: &str) -> crate::vm::Value {
        let bytes = s.as_bytes();
        match crate::vm::ActorVmCallbacks::alloc(
            self,
            bytes.len() + 1,
            crate::runtime::heap::TypeTag::String,
        ) {
            Some(ptr) => {
                // SAFETY: `alloc` just returned a fresh allocation of
                // exactly `bytes.len() + 1` bytes; writing `bytes.len()`
                // payload bytes plus a trailing NUL fits exactly.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                    *ptr.add(bytes.len()) = 0;
                }
                crate::vm::Value::ptr(ptr)
            }
            None => crate::vm::Value::nil(),
        }
    }
}

impl std::fmt::Debug for RuntimeVmCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeVmCallbacks").finish_non_exhaustive()
    }
}

impl crate::vm::ActorVmCallbacks for RuntimeVmCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        self.runtime.borrow().current_actor
    }

    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {
        let rt = self.runtime.borrow();
        authorize_actor_ffi(&rt, rt.current_actor, library, symbol).is_ok()
    }

    fn alloc(&mut self, size: usize, type_tag: crate::runtime::heap::TypeTag) -> Option<*mut u8> {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                return actor.heap.alloc(size, type_tag);
            }
        }
        // No actor context (e.g. `main()`'s own top-level bytecode): fall
        // back to the runtime's dedicated main heap rather than silently
        // failing every allocation. See `Runtime::main_heap`'s doc comment.
        rt.main_heap.alloc(size, type_tag)
    }

    // SAFETY: trait-impl signature is fixed; `ptr` always comes from the
    // VM's own heap allocations (the current actor's ActorHeap, or the
    // runtime's main heap when there is no current actor).
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn drop_ref(&mut self, ptr: *mut u8) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                // Route through ORCA so objects with outstanding foreign
                // references are deferred instead of freed out from under
                // other actors.
                unsafe {
                    actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
                }
                return;
            }
        }
        unsafe {
            let rt = &mut *rt;
            rt.main_gc.drop_local_ref(&mut rt.main_heap, ptr);
        }
    }

    // SAFETY: trait-impl signature is fixed; `ptr` always comes from the
    // VM's own heap allocations (the current actor's ActorHeap, or the
    // runtime's main heap when there is no current actor).
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn retain_ref(&mut self, ptr: *mut u8) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                unsafe {
                    actor.orca_gc.local_ref(&actor.heap, ptr);
                }
                return;
            }
        }
        unsafe {
            let rt = &mut *rt;
            rt.main_gc.local_ref(&rt.main_heap, ptr);
        }
    }

    // SAFETY: trait-impl signature is fixed; `ptr` always comes from the
    // VM's own heap allocations (the current actor's ActorHeap, or the
    // runtime's main heap when there is no current actor). `header_of` is a
    // pure pointer-arithmetic read relative to `ptr` itself, so it needs no
    // actor/heap lookup at all beyond confirming there's a valid execution
    // context to be reading from.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let header = &*crate::runtime::heap::ActorHeap::header_of(ptr);
            if header.type_tag == crate::runtime::heap::TypeTag::Array {
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
        spawn_pc: usize,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        let mut rt = self.runtime.borrow_mut();
        spawn_with_site_authority(&mut rt, module, spawn_pc, behavior_idx, init)
    }

    fn send_message(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) {
        if let Some(actor_id) = target.as_actor_id() {
            let mut rt = self.runtime.borrow_mut();
            rt.send_message_by_id(actor_id, behavior_id, args);
        }
    }

    fn ask_actor(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) -> crate::vm::Value {
        if let Some(actor_id) = target.as_actor_id() {
            let mut rt = self.runtime.borrow_mut();
            match rt.ask_actor_sync(actor_id, behavior_id, args) {
                Ok(value) => return value,
                Err(_) => {}
            }
        }
        crate::vm::Value::nil()
    }

    fn get_state_field(&self, field: &str) -> crate::vm::Value {
        let rt = self.runtime.borrow();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get(&actor_id) {
                return actor
                    .get_state_field(field)
                    .unwrap_or(crate::vm::Value::nil());
            }
        }
        crate::vm::Value::nil()
    }

    fn set_state_field(&mut self, field: &str, value: crate::vm::Value) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                // CRDT-backed fields mutate only through the `Crdt.*` effect
                // module; a raw `self.field = expr` assignment is ignored so it
                // cannot silently orphan `state_data` from the replicated entry.
                if actor
                    .state_models
                    .get(field)
                    .map(|m| m.is_crdt())
                    .unwrap_or(false)
                {
                    tracing::warn!(
                        "nulang-crdt: ignoring raw assignment to CRDT field '{}' on actor {}; \
                         use the Crdt.* effect module instead",
                        field,
                        actor.id
                    );
                    return;
                }
                actor.set_state_field(field, value);
            }
        }
    }

    fn emit_event(&mut self, event: &str, args: &[crate::vm::Value]) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            rt.emit_event(actor_id, event, args);
        }
    }

    fn perform_effect(
        &mut self,
        effect_name: &str,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        if effect_name != "Timer" {
            return None;
        }
        let mut rt = self.runtime.borrow_mut();
        let actor_id = rt.current_actor?;
        if !rt.actor_is_workflow(actor_id) {
            return Some(crate::vm::Value::unit());
        }
        let name = {
            let vm = rt.vm.as_mut()?;
            let module_idx = vm.current_module_idx()?;
            let string_id = regs.get(0)?.as_string_id()?;
            vm.constant_string(module_idx, string_id)?
        };
        let duration_ms = regs.get(1)?.as_int()? as u64;
        rt.schedule_workflow_timer(actor_id, &name, duration_ms);
        Some(crate::vm::Value::unit())
    }

    #[cfg_attr(not(feature = "ai-runtime"), allow(unused_variables))]
    fn perform_builtin_effect(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        if effect_name == "Workflow" && op_name == Some("query") {
            let workflow_id = regs.get(0)?.as_actor_id()?;
            let string_id = regs.get(1)?.as_string_id()?;
            let query_name = match constants.get(string_id as usize) {
                Some(crate::bytecode::Constant::String(s)) => s.clone(),
                _ => return None,
            };
            let mut rt = self.runtime.borrow_mut();
            return rt.query_workflow(workflow_id, &query_name);
        }
        #[cfg(feature = "sqlite")]
        if effect_name == "DB" && op_name == Some("query") {
            let sql = match regs.first().and_then(|v| v.as_string_id()) {
                Some(id) => match constants.get(id as usize) {
                    Some(crate::bytecode::Constant::String(s)) => s.clone(),
                    _ => return Some(crate::vm::Value::nil()),
                },
                None => return Some(crate::vm::Value::nil()),
            };
            let params: Vec<crate::vm::Value> = regs.iter().skip(1).copied().collect();
            let rt = self.runtime.borrow_mut();
            let query_result = rt.persistence.query(&sql, &params);
            drop(rt);
            let result = match query_result {
                Ok(rows) => {
                    let json = serde_json::to_string(&rows).unwrap_or_default();
                    self.alloc_string(&json)
                }
                Err(_) => crate::vm::Value::nil(),
            };
            return Some(result);
        }
        if effect_name == "Timer" && op_name == Some("after") {
            let ms = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            if ms > 0 {
                let callback_id = regs.get(1).and_then(|v| v.as_string_id());
                let callback_name = callback_id.and_then(|id| {
                    constants.get(id as usize).and_then(|c| match c {
                        crate::bytecode::Constant::String(s) => Some(s.clone()),
                        _ => None,
                    })
                });
                if let Some(callback_name) = callback_name {
                    let rt = self.runtime.borrow_mut();
                    let actor_id = rt.current_actor.unwrap_or(0);
                    let behavior_id = rt.behavior_id_for(actor_id, &callback_name).unwrap_or(0);
                    if behavior_id > 0 {
                        rt.timer_wheel.send_after(
                            std::time::Duration::from_millis(ms as u64),
                            actor_id,
                            behavior_id,
                            vec![],
                        );
                    }
                }
            }
            return Some(crate::vm::Value::unit());
        }
        if effect_name == "Int" && op_name == Some("to_string") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = format!("{}", n);
            return Some(self.alloc_string(&s));
        }

        if effect_name == "Int" && op_name == Some("to_float") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            return Some(crate::vm::Value::float(n as f64));
        }
        if effect_name == "Float" && op_name == Some("to_int") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(crate::vm::Value::int(x as i64));
        }
        if effect_name == "Float" && op_name == Some("to_string") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            let s = format!("{}", x);
            return Some(self.alloc_string(&s));
        }
        if effect_name == "String" && op_name == Some("to_int") {
            let s = crate::vm::resolve_value_string(
                constants,
                *regs.first().unwrap_or(&crate::vm::Value::nil()),
            );
            let n: i64 = s.parse().unwrap_or(0);
            return Some(crate::vm::Value::int(n));
        }
        if effect_name == "String" && op_name == Some("to_float") {
            let s = crate::vm::resolve_value_string(
                constants,
                *regs.first().unwrap_or(&crate::vm::Value::nil()),
            );
            let f: f64 = s.parse().unwrap_or(0.0);
            return Some(crate::vm::Value::float(f));
        }

        if effect_name == "String" && op_name == Some("length") {
            let s = crate::vm::resolve_value_string(
                constants,
                *regs.first().unwrap_or(&crate::vm::Value::nil()),
            );
            return Some(crate::vm::Value::int(s.len() as i64));
        }
        if effect_name == "String" && op_name == Some("charAt") {
            let s = crate::vm::resolve_value_string(
                constants,
                *regs.first().unwrap_or(&crate::vm::Value::nil()),
            );
            let idx = regs.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
            if idx < 0 || idx as usize >= s.len() {
                return Some(crate::vm::Value::int(-1));
            }
            return Some(crate::vm::Value::int(s.as_bytes()[idx as usize] as i64));
        }
        if effect_name == "Provider" && op_name == Some("ask") {
            // General runtime-registered provider dispatch. The first arg is
            // the provider name (string); the second is the prompt/request
            // (string). This is the longevity path: `perform Provider.ask`
            // references no transient technology, only an eternal "provider"
            // abstraction. The "llm" provider reuses the existing LLM client.
            let provider = match regs.get(0).and_then(|v| v.as_string_id()) {
                Some(id) => match constants.get(id as usize) {
                    Some(crate::bytecode::Constant::String(s)) => s.clone(),
                    _ => return None,
                },
                None => return None,
            };
            let prompt = match regs.get(1) {
                Some(v) => {
                    if let Some(id) = v.as_string_id() {
                        constants
                            .get(id as usize)
                            .and_then(|c| match c {
                                crate::bytecode::Constant::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .unwrap_or_default()
                    } else {
                        v.to_string_repr()
                    }
                }
                None => return None,
            };
            if provider == "llm" {
                #[cfg(feature = "ai-runtime")]
                {
                    let rt = self.runtime.borrow_mut();
                    if rt.llm.client.is_none() {
                        return Some(crate::vm::Value::nil());
                    }
                    let request = nulang_ai::LlmRequest {
                        model: String::new(),
                        messages: vec![nulang_ai::LlmMessage {
                            role: "user".to_string(),
                            content: prompt,
                        }],
                        tools: Vec::new(),
                        memory: Vec::new(),
                        pricing: None,
                        response_format: None,
                    };
                    let result = rt.complete_llm_request(request, Vec::new());
                    drop(rt);
                    return Some(match result {
                        Ok(resp) => match resp.content {
                            Some(c) => self.alloc_string(&c),
                            None => crate::vm::Value::nil(),
                        },
                        Err(_) => crate::vm::Value::nil(),
                    });
                }
                #[cfg(not(feature = "ai-runtime"))]
                {
                    return Some(crate::vm::Value::nil());
                }
            }
            return None;
        }
        if effect_name == "Debug" && op_name == Some("inspect") {
            let target_id = regs.first().and_then(|v| v.as_int()).unwrap_or(0) as u64;
            let rt = self.runtime.borrow();
            let info = serde_json::json!({
                "state": rt.actors.get(&target_id).map(|a| {
                    a.state_data.iter().map(|(k, v)| {
                        (k.clone(), crate::vm::resolve_value_string(constants, *v))
                    }).collect::<std::collections::HashMap<_, _>>()
                }).unwrap_or_default(),
                "mailbox_size": rt.actors.get(&target_id).map(|a| a.mailbox.len()).unwrap_or(0),
                "behaviors": rt.actors.get(&target_id).map(|a| {
                    a.behavior_table.iter().map(|b| b.name.clone()).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "supervisor": rt.supervisors.get(&target_id).map(|_s| target_id),
            });
            drop(rt);
            let json = serde_json::to_string(&info).unwrap_or_default();
            return Some(self.alloc_string(&json));
        }
        if effect_name == "Actor" {
            let mut rt = self.runtime.borrow_mut();
            let actor_id = rt.current_actor;
            return rt.perform_actor_builtin(actor_id, op_name, constants, regs);
        }
        if effect_name == "Grain" {
            let mut rt = self.runtime.borrow_mut();
            return rt.perform_grain_builtin(op_name, constants, regs);
        }
        if effect_name == "IO" {
            if let (Some("print") | Some("println"), Some(first)) = (op_name, regs.first()) {
                let msg = crate::vm::resolve_value_string(constants, *first);
                println!("{}", msg);
                return Some(crate::vm::Value::unit());
            }
        }
        #[cfg(feature = "python")]
        if effect_name == "Python" {
            let mut rt = self.runtime.borrow_mut();
            return rt.perform_python_builtin(op_name, constants, regs);
        }
        if effect_name == "Crdt" {
            let mut rt = self.runtime.borrow_mut();
            let actor_id = rt.current_actor;
            return rt.perform_crdt_builtin(actor_id, op_name, constants, regs);
        }
        if effect_name == "Web" {
            return perform_web_builtin(self, op_name, constants, regs);
        }
        if effect_name == "Realtime" {
            return perform_realtime_builtin(self, op_name, constants, regs);
        }
        self.perform_effect(effect_name, regs)
    }

    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &crate::bytecode::CodeModule,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        let qualified = match op_name {
            Some(op) => format!("{}.{}", effect_name, op),
            None => effect_name.to_string(),
        };
        // Check test handlers before real dispatch — allows tests to
        // intercept effects without a `handle` block in source.
        {
            let rt = self.runtime.borrow();
            if let Some(result) = rt.check_test_handler(&qualified, regs) {
                return Some(result);
            }
            if let Err(error) = authorize_actor_host_effect(
                &rt,
                rt.current_actor,
                effect_name,
                op_name,
                &module.constants,
                regs,
            ) {
                tracing::warn!(
                    actor_id = ?rt.current_actor,
                    effect = %qualified,
                    %error,
                    "denying actor-backed host effect"
                );
                return Some(crate::vm::Value::nil());
            }
        }
        if effect_name == "Otp" {
            let mut rt = self.runtime.borrow_mut();
            return rt.perform_otp_builtin(op_name, module, regs);
        }
        if effect_name == "Http" && op_name == Some("serve") {
            let port = regs.first().and_then(|v| v.as_int()).unwrap_or(0) as u16;
            let func_idx = match regs.get(1) {
                Some(v) if v.is_closure() => {
                    let payload = v.as_raw() & crate::value_layout::PAYLOAD_MASK;
                    if payload & crate::vm::CLOSURE_ENV_FLAG != 0 {
                        return Some(crate::vm::Value::nil());
                    }
                    payload as usize
                }
                Some(v) => {
                    // Function index passed as raw Int (from func_map lookup).
                    v.as_int().unwrap_or(0) as usize
                }
                None => return Some(crate::vm::Value::nil()),
            };
            return match HttpServerState::bind(port, module.clone(), func_idx) {
                Ok(server) => {
                    let actual_port = server.port;
                    self.runtime.borrow_mut().http_server = Some(server);
                    Some(crate::vm::Value::int(actual_port as i64))
                }
                Err(_) => Some(crate::vm::Value::nil()),
            };
        }
        self.perform_builtin_effect(effect_name, op_name, &module.constants, regs)
    }

    #[cfg_attr(not(feature = "ai-runtime"), allow(unused_variables))]
    fn perform_async(
        &mut self,
        effect_op: &str,
        constants: &[crate::bytecode::Constant],
        args: &[crate::vm::Value],
    ) -> crate::vm::PerformAsyncResult {
        match effect_op {
            #[cfg(feature = "ai-runtime")]
            "Inference.ask" | "LLM.ask" => {
                let prompt = resolve_first_string(constants, args);
                let result = self.complete_llm("", &prompt);
                crate::vm::PerformAsyncResult::Ready(result)
            }
            "Timer.sleep" => {
                let ms = args.first().and_then(|v| v.as_int()).unwrap_or(0) as u64;
                let mut rt = self.runtime.borrow_mut();
                let actor_id = rt.current_actor.unwrap_or(0);
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    if actor.timer_sleep_fired {
                        actor.timer_sleep_fired = false;
                        return crate::vm::PerformAsyncResult::Ready(None);
                    }
                }
                if ms == 0 {
                    return crate::vm::PerformAsyncResult::Ready(None);
                }
                if ms > 0 {
                    rt.timer_wheel
                        .timer_sleep_wake(std::time::Duration::from_millis(ms), actor_id);
                }
                crate::vm::PerformAsyncResult::Pending
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.new" => {
                let id = self.runtime.borrow_mut().pipeline_new();
                crate::vm::PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.stage" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let actor = actor_arg(args, 2);
                let template = string_arg(constants, args, 3);
                let result = self
                    .runtime
                    .borrow_mut()
                    .pipeline_stage(id, &name, actor, &template);
                let r = result.map(|id| id as i64).unwrap_or(-1);
                crate::vm::PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.run" => {
                let id = id_arg(constants, args, 0);
                let input = string_arg(constants, args, 1);
                let result = self.runtime.borrow_mut().pipeline_run(id, &input).ok();
                crate::vm::PerformAsyncResult::Ready(result)
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.new" => {
                let id = self.runtime.borrow_mut().supervisor_new();
                crate::vm::PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.worker" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let actor = actor_arg(args, 2);
                let description = string_arg(constants, args, 3);
                let result =
                    self.runtime
                        .borrow_mut()
                        .supervisor_worker(id, &name, actor, &description);
                let r = result.map(|id| id as i64).unwrap_or(-1);
                crate::vm::PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.run" => {
                let id = id_arg(constants, args, 0);
                let task = string_arg(constants, args, 1);
                let result = self.runtime.borrow_mut().supervisor_run(id, &task).ok();
                crate::vm::PerformAsyncResult::Ready(result)
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.new" => {
                let topic = string_arg(constants, args, 0);
                let rounds = int_arg(args, 1);
                let threshold = float_arg(args, 2);
                let id = self
                    .runtime
                    .borrow_mut()
                    .debate_new(&topic, rounds, threshold);
                crate::vm::PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.participant" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let stance = string_arg(constants, args, 2);
                let actor = actor_arg(args, 3);
                let result = self
                    .runtime
                    .borrow_mut()
                    .debate_participant(id, &name, &stance, actor);
                let r = result.map(|id| id as i64).unwrap_or(-1);
                crate::vm::PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.run" => {
                let id = id_arg(constants, args, 0);
                let result = self.runtime.borrow_mut().debate_run(id).ok();
                crate::vm::PerformAsyncResult::Ready(result)
            }
            _ => crate::vm::PerformAsyncResult::Ready(None),
        }
    }

    #[cfg(feature = "ai-runtime")]
    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {
        let mut rt = self.runtime.borrow_mut();
        if authorize_actor_host_effect(
            &rt,
            rt.current_actor,
            "Inference",
            Some("ask"),
            &[],
            &[],
        )
        .is_err()
        {
            return None;
        }
        if let Some(actor_id) = rt.current_actor {
            if rt
                .actors
                .get(&actor_id)
                .map(|a| a.is_agent)
                .unwrap_or(false)
            {
                return rt.complete_agent_llm(actor_id, prompt);
            }
        }
        // Top-level (non-actor) LLM ask: issue a direct request without
        // agent state or memory handling.
        let request = LlmRequest {
            model: model.to_string(),
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            tools: Vec::new(),
            memory: Vec::new(),
            pricing: None,
            response_format: None,
        };
        rt.complete_llm_request(request, Vec::new()).ok()?.content
    }

    fn try_receive(&mut self) -> Option<(u16, crate::vm::Value)> {
        let mut rt = self.runtime.borrow_mut();
        let actor_id = rt.current_actor?;
        let msg = rt.actors.get_mut(&actor_id)?.mailbox.pop()?;
        // ORCA receiver protocol: hold heap pointers carried by the message.
        rt.hold_payload_refs(actor_id, &*msg.payload);
        let val = msg
            .payload
            .first()
            .cloned()
            .unwrap_or(crate::vm::Value::unit());
        Some((msg.behavior_id, val))
    }

    fn try_receive_match(
        &mut self,
        behavior_ids: &[u16],
    ) -> Option<(usize, Vec<crate::vm::Value>)> {
        let mut rt = self.runtime.borrow_mut();
        let actor_id = rt.current_actor?;
        let (pos, payload) = rt
            .actors
            .get_mut(&actor_id)?
            .mailbox
            .receive_match(behavior_ids)?;
        // ORCA receiver protocol: hold heap pointers carried by the message.
        rt.hold_payload_refs(actor_id, &*payload);
        Some((
            pos,
            Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),
        ))
    }

    fn commit_receive_match(&mut self) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.mailbox.commit_receive_match();
            }
        }
    }

    fn reset_receive_match(&mut self) {
        let mut rt = self.runtime.borrow_mut();
        if let Some(actor_id) = rt.current_actor {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.mailbox.reset_receive_match();
            }
        }
    }
}

// Helpers for extracting typed arguments from PerformAsync register values.
#[cfg(feature = "ai-runtime")]
fn int_arg(args: &[crate::vm::Value], idx: usize) -> i64 {
    args.get(idx).and_then(|v| v.as_int()).unwrap_or(0)
}

#[cfg(feature = "ai-runtime")]
fn actor_arg(args: &[crate::vm::Value], idx: usize) -> u64 {
    args.get(idx).and_then(|v| v.as_actor_id()).unwrap_or(0)
}

#[cfg(feature = "ai-runtime")]
fn float_arg(args: &[crate::vm::Value], idx: usize) -> f64 {
    args.get(idx).and_then(|v| v.as_float()).unwrap_or(0.0)
}

#[cfg(feature = "ai-runtime")]
fn string_arg(
    constants: &[crate::bytecode::Constant],
    args: &[crate::vm::Value],
    idx: usize,
) -> String {
    args.get(idx).map_or(String::new(), |v| {
        if let Some(s) = v.as_string_id() {
            constants
                .get(s as usize)
                .and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            String::new()
        }
    })
}

#[cfg(feature = "ai-runtime")]
fn resolve_first_string(
    constants: &[crate::bytecode::Constant],
    args: &[crate::vm::Value],
) -> String {
    string_arg(constants, args, 0)
}

#[cfg(feature = "ai-runtime")]
fn id_arg(constants: &[crate::bytecode::Constant], args: &[crate::vm::Value], idx: usize) -> u64 {
    // Try int first (legacy path), then parse string-id from constants as u64.
    if let Some(v) = args.get(idx) {
        if let Some(n) = v.as_int() {
            return n as u64;
        }
    }
    let s = string_arg(constants, args, idx);
    s.parse::<u64>().unwrap_or(0)
}

/// Raw-pointer callbacks used when the runtime itself executes an actor's
/// bytecode behavior. Holds a transient borrow of the executing `Runtime`.
#[derive(Debug)]
pub(crate) struct BytecodeRuntimeCallbacks {
    runtime: *mut Runtime,
    actor_id: u64,
}

// SAFETY: `runtime` is a transient borrow of the executing `Runtime` that
// is valid for the duration of the behavior invocation. The scheduler
// guarantees that a `Runtime` (and thus each callback instance wrapping a
// pointer to it) is only driven from one thread at a time, so no two
// threads can alias the `&mut Runtime` produced by dereferencing `runtime`.
unsafe impl Send for BytecodeRuntimeCallbacks {}
// SAFETY: shared references only grant access through `Sync` if methods can
// be called concurrently; all callback methods mutate through the raw
// pointer and are only invoked while the owning thread is executing the
// behavior, so cross-thread concurrent use cannot occur by construction.
unsafe impl Sync for BytecodeRuntimeCallbacks {}

impl BytecodeRuntimeCallbacks {
    pub(crate) fn new(runtime: *mut Runtime, actor_id: u64) -> Self {
        BytecodeRuntimeCallbacks { runtime, actor_id }
    }

    fn authority_actor_id(&self) -> Option<u64> {
        (self.actor_id != 0).then_some(self.actor_id)
    }
}

impl crate::vm::ActorVmCallbacks for BytecodeRuntimeCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        Some(self.actor_id)
    }

    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {
        unsafe {
            authorize_actor_ffi(&*self.runtime, self.authority_actor_id(), library, symbol).is_ok()
        }
    }

    fn alloc(&mut self, size: usize, type_tag: crate::runtime::heap::TypeTag) -> Option<*mut u8> {
        unsafe {
            (*self.runtime)
                .actors
                .get_mut(&self.actor_id)?
                .heap
                .alloc(size, type_tag)
        }
    }

    fn alloc_arena(
        &mut self,
        size: usize,
        type_tag: crate::runtime::heap::TypeTag,
    ) -> Option<*mut u8> {
        unsafe {
            (*self.runtime)
                .actors
                .get_mut(&self.actor_id)?
                .iso_arena
                .alloc(size, type_tag)
        }
    }

    fn reset_arena(&mut self) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.iso_arena.reset();
            }
        }
    }

    fn is_arena_ptr(&self, ptr: *const u8) -> bool {
        unsafe {
            (*self.runtime)
                .actors
                .get(&self.actor_id)
                .map(|a| a.iso_arena.contains(ptr))
                .unwrap_or(false)
        }
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                // Arena objects are reclaimed wholesale at activation end and
                // are not on the heap live list: rc traffic on them is a no-op.
                if actor.iso_arena.contains(ptr) {
                    return;
                }
                // Route through ORCA so objects with outstanding foreign
                // references are deferred instead of freed out from under
                // other actors.
                actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
            }
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                if actor.iso_arena.contains(ptr) {
                    return;
                }
                actor.orca_gc.local_ref(&actor.heap, ptr);
            }
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let _actor = (*self.runtime).actors.get(&self.actor_id)?;
            let header = &*crate::runtime::heap::ActorHeap::header_of(ptr);
            if header.type_tag == crate::runtime::heap::TypeTag::Array {
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
        spawn_pc: usize,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        // SAFETY: the callback is installed on the shared runtime VM only
        // while the runtime drives a behavior on the single scheduler
        // thread, so `runtime` is a live, exclusively-borrowed pointer.
        // Spawning mutates runtime state but never re-enters the VM.
        unsafe {
            spawn_with_site_authority(&mut *self.runtime, module, spawn_pc, behavior_idx, init)
        }
    }

    fn send_message(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) {
        if let Some(target_id) = target.as_actor_id() {
            // SAFETY: as above. `send_message_by_id` is safe mid-behavior:
            // it pushes mail, bumps ORCA foreign counts, and enqueues the
            // target; the receive-wait wake is deferred while the shared
            // VM is executing (see `Runtime::pending_receive_wakes`).
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
                // CRDT-backed fields mutate only through the `Crdt.*` effect
                // module; a raw `self.field = expr` assignment is ignored so it
                // cannot silently orphan `state_data` from the replicated entry.
                if actor
                    .state_models
                    .get(field)
                    .map(|m| m.is_crdt())
                    .unwrap_or(false)
                {
                    tracing::warn!(
                        "nulang-crdt: ignoring raw assignment to CRDT field '{}' on actor {}; \
                         use the Crdt.* effect module instead",
                        field,
                        actor.id
                    );
                    return;
                }
                actor.set_state_field(field, value);
            }
        }
    }

    fn emit_event(&mut self, event: &str, args: &[crate::vm::Value]) {
        unsafe {
            (*self.runtime).emit_event(self.actor_id, event, args);
        }
    }

    fn wait_signal(&mut self, name: &str) -> crate::vm::SignalWaitResult {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get(&self.actor_id) {
                if actor.received_signals.iter().any(|(n, _)| n == name) {
                    return crate::vm::SignalWaitResult::Ready(crate::vm::Value::unit());
                }
            }
            crate::vm::SignalWaitResult::NotReady
        }
    }

    fn suspend_for_signal(&mut self, _name: &str, _vm_state: Option<crate::vm::SuspendedVmState>) {
        // State capture is handled by run_bytecode_at_offset after run_from
        // returns, avoiding aliasing the Runtime through this raw-pointer
        // callback while the VM borrow is active.
    }

    fn perform_effect(
        &mut self,
        effect_name: &str,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        unsafe {
            if effect_name != "Timer" {
                return None;
            }
            let actor = (*self.runtime).actors.get(&self.actor_id)?;
            if !actor.is_workflow {
                return Some(crate::vm::Value::unit());
            }
            let vm = (*self.runtime).vm.as_mut()?;
            let module_idx = vm.current_module_idx()?;
            let string_id = regs.get(0)?.as_string_id()?;
            let name = vm.constant_string(module_idx, string_id)?;
            let duration_ms = regs.get(1)?.as_int()? as u64;
            (*self.runtime).schedule_workflow_timer(self.actor_id, &name, duration_ms);
            Some(crate::vm::Value::unit())
        }
    }

    #[cfg_attr(not(feature = "ai-runtime"), allow(unused_variables))]
    fn perform_builtin_effect(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        unsafe {
            if effect_name == "Workflow" && op_name == Some("query") {
                let workflow_id = regs.get(0)?.as_actor_id()?;
                let string_id = regs.get(1)?.as_string_id()?;
                let query_name = match constants.get(string_id as usize) {
                    Some(crate::bytecode::Constant::String(s)) => s.clone(),
                    _ => return None,
                };
                return (*self.runtime).query_workflow(workflow_id, &query_name);
            }
            #[cfg(feature = "sqlite")]
            if effect_name == "DB" && op_name == Some("query") {
                let sql = match regs.first().and_then(|v| v.as_string_id()) {
                    Some(id) => match constants.get(id as usize) {
                        Some(crate::bytecode::Constant::String(s)) => s.clone(),
                        _ => return Some(crate::vm::Value::nil()),
                    },
                    None => return Some(crate::vm::Value::nil()),
                };
                let params: Vec<crate::vm::Value> = regs.iter().skip(1).copied().collect();
                return match (*self.runtime).persistence.query(&sql, &params) {
                    Ok(rows) => {
                        let json = serde_json::to_string(&rows).unwrap_or_default();
                        if let Some(vm) = &mut (*self.runtime).vm {
                            Some(vm.allocate_string(&json))
                        } else {
                            Some(crate::vm::Value::nil())
                        }
                    }
                    Err(_) => Some(crate::vm::Value::nil()),
                };
            }
            if effect_name == "Actor" {
                return (*self.runtime).perform_actor_builtin(
                    Some(self.actor_id),
                    op_name,
                    constants,
                    regs,
                );
            }

            if effect_name == "Grain" {
                return (*self.runtime).perform_grain_builtin(op_name, constants, regs);
            }

            if effect_name == "Crdt" {
                return (*self.runtime).perform_crdt_builtin(
                    Some(self.actor_id),
                    op_name,
                    constants,
                    regs,
                );
            }

            if effect_name == "Int" && op_name == Some("to_float") {
                let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
                return Some(crate::vm::Value::float(n as f64));
            }
            if effect_name == "Float" && op_name == Some("to_int") {
                let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
                return Some(crate::vm::Value::int(x as i64));
            }
            if effect_name == "Float" && op_name == Some("to_string") {
                let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
                let s = format!("{}", x);
                if let Some(vm) = &mut (*self.runtime).vm {
                    return Some(vm.allocate_string(&s));
                }
                return Some(crate::vm::Value::nil());
            }
            if effect_name == "String" && op_name == Some("to_int") {
                let s = crate::vm::resolve_value_string(
                    constants,
                    *regs.first().unwrap_or(&crate::vm::Value::nil()),
                );
                let n: i64 = s.parse().unwrap_or(0);
                return Some(crate::vm::Value::int(n));
            }
            if effect_name == "String" && op_name == Some("to_float") {
                let s = crate::vm::resolve_value_string(
                    constants,
                    *regs.first().unwrap_or(&crate::vm::Value::nil()),
                );
                let f: f64 = s.parse().unwrap_or(0.0);
                return Some(crate::vm::Value::float(f));
            }
            if effect_name == "Timer" && op_name == Some("after") {
                let ms = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
                if ms > 0 {
                    let callback_id = regs.get(1).and_then(|v| v.as_string_id());
                    let callback_name = callback_id.and_then(|id| {
                        constants.get(id as usize).and_then(|c| match c {
                            crate::bytecode::Constant::String(s) => Some(s.clone()),
                            _ => None,
                        })
                    });
                    if let Some(callback_name) = callback_name {
                        let behavior_id = (*self.runtime)
                            .behavior_id_for(self.actor_id, &callback_name)
                            .unwrap_or(0);
                        if behavior_id > 0 {
                            (*self.runtime).timer_wheel.send_after(
                                std::time::Duration::from_millis(ms as u64),
                                self.actor_id,
                                behavior_id,
                                vec![],
                            );
                        }
                    }
                }
                return Some(crate::vm::Value::unit());
            }
            if effect_name == "Int" && op_name == Some("to_string") {
                let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
                let s = format!("{}", n);
                if let Some(vm) = &mut (*self.runtime).vm {
                    return Some(vm.allocate_string(&s));
                }
                return Some(crate::vm::Value::nil());
            }

            if effect_name == "String" && op_name == Some("length") {
                let s = crate::vm::resolve_value_string(
                    constants,
                    *regs.first().unwrap_or(&crate::vm::Value::nil()),
                );
                return Some(crate::vm::Value::int(s.len() as i64));
            }
            if effect_name == "String" && op_name == Some("charAt") {
                let s = crate::vm::resolve_value_string(
                    constants,
                    *regs.first().unwrap_or(&crate::vm::Value::nil()),
                );
                let idx = regs.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
                if idx < 0 || idx as usize >= s.len() {
                    return Some(crate::vm::Value::int(-1));
                }
                return Some(crate::vm::Value::int(s.as_bytes()[idx as usize] as i64));
            }
            if effect_name == "Provider" && op_name == Some("ask") {
                // General runtime-registered provider dispatch (actor path).
                // Mirrors RuntimeVmCallbacks::perform_builtin_effect's Provider
                // branch. The "llm" provider reuses the agent-aware complete_llm.
                let provider = match regs.get(0).and_then(|v| v.as_string_id()) {
                    Some(id) => match constants.get(id as usize) {
                        Some(crate::bytecode::Constant::String(s)) => s.clone(),
                        _ => return None,
                    },
                    None => return None,
                };
                let prompt = match regs.get(1) {
                    Some(v) => {
                        if let Some(id) = v.as_string_id() {
                            constants
                                .get(id as usize)
                                .and_then(|c| match c {
                                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                                    _ => None,
                                })
                                .unwrap_or_default()
                        } else {
                            v.to_string_repr()
                        }
                    }
                    None => return None,
                };
                if provider == "llm" {
                    #[cfg(feature = "ai-runtime")]
                    {
                        let content = self.complete_llm("", &prompt);
                        let rt = &mut *self.runtime;
                        return Some(match content {
                            Some(c) => match &mut rt.vm {
                                Some(vm) => vm.allocate_string(&c),
                                None => crate::vm::Value::nil(),
                            },
                            None => crate::vm::Value::nil(),
                        });
                    }
                    #[cfg(not(feature = "ai-runtime"))]
                    {
                        return Some(crate::vm::Value::nil());
                    }
                }
            }
            if effect_name == "Debug" && op_name == Some("inspect") {
                let target_id = regs.first().and_then(|v| v.as_int()).unwrap_or(0) as u64;
                let rt = &mut *self.runtime;
                let info = serde_json::json!({
                    "state": rt.actors.get(&target_id).map(|a| {
                        a.state_data.iter().map(|(k, v)| {
                            (k.clone(), crate::vm::resolve_value_string(constants, *v))
                        }).collect::<std::collections::HashMap<_, _>>()
                    }).unwrap_or_default(),
                    "mailbox_size": rt.actors.get(&target_id).map(|a| a.mailbox.len()).unwrap_or(0),
                    "behaviors": rt.actors.get(&target_id).map(|a| {
                        a.behavior_table.iter().map(|b| b.name.clone()).collect::<Vec<_>>()
                    }).unwrap_or_default(),
                    "supervisor": rt.supervisors.get(&target_id).map(|_s| target_id),
                });
                let json = serde_json::to_string(&info).unwrap_or_default();
                if let Some(vm) = &mut rt.vm {
                    return Some(vm.allocate_string(&json));
                }
                return Some(crate::vm::Value::nil());
            }
            if effect_name == "IO" {
                if let (Some("print") | Some("println"), Some(first)) = (op_name, regs.first()) {
                    let msg = crate::vm::resolve_value_string(constants, *first);
                    println!("{}", msg);
                    return Some(crate::vm::Value::unit());
                }
            }
            if effect_name == "Web" {
                return perform_web_builtin(self, op_name, constants, regs);
            }
            if effect_name == "Realtime" {
                return perform_realtime_builtin(self, op_name, constants, regs);
            }
            if effect_name == "StrBuilder" {
                return crate::vm::strbuilder_op(self, constants, op_name.unwrap_or(""), regs);
            }
            if effect_name == "Map" {
                return crate::vm::hashmap_op(self, constants, op_name.unwrap_or(""), regs);
            }
            self.perform_effect(effect_name, regs)
        }
    }

    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &crate::bytecode::CodeModule,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        let qualified = match op_name {
            Some(op) => format!("{}.{}", effect_name, op),
            None => effect_name.to_string(),
        };
        unsafe {
            // Check test handlers before real dispatch.
            if let Some(result) = (*self.runtime).check_test_handler(&qualified, regs) {
                return Some(result);
            }
            if let Err(error) = authorize_actor_host_effect(
                &*self.runtime,
                self.authority_actor_id(),
                effect_name,
                op_name,
                &module.constants,
                regs,
            ) {
                tracing::warn!(
                    actor_id = self.actor_id,
                    effect = %qualified,
                    %error,
                    "denying actor-backed host effect"
                );
                return Some(crate::vm::Value::nil());
            }
            if effect_name == "Otp" {
                return (*self.runtime).perform_otp_builtin(op_name, module, regs);
            }
            if effect_name == "Http" && op_name == Some("serve") {
                let port = regs.first().and_then(|v| v.as_int()).unwrap_or(0) as u16;
                let func_idx = match regs.get(1) {
                    Some(v) if v.is_closure() => {
                        let payload = v.as_raw() & crate::value_layout::PAYLOAD_MASK;
                        if payload & crate::vm::CLOSURE_ENV_FLAG != 0 {
                            return Some(crate::vm::Value::nil());
                        }
                        payload as usize
                    }
                    Some(v) => v.as_int().unwrap_or(0) as usize,
                    None => return Some(crate::vm::Value::nil()),
                };
                return match HttpServerState::bind(port, module.clone(), func_idx) {
                    Ok(server) => {
                        let actual_port = server.port;
                        (*self.runtime).http_server = Some(server);
                        Some(crate::vm::Value::int(actual_port as i64))
                    }
                    Err(_) => Some(crate::vm::Value::nil()),
                };
            }
            self.perform_builtin_effect(effect_name, op_name, &module.constants, regs)
        }
    }

    #[cfg(feature = "ai-runtime")]
    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {
        unsafe {
            let rt = &mut *self.runtime;
            if authorize_actor_host_effect(
                rt,
                self.authority_actor_id(),
                "Inference",
                Some("ask"),
                &[],
                &[],
            )
            .is_err()
            {
                return None;
            }
            if rt
                .actors
                .get(&self.actor_id)
                .map(|a| a.is_agent)
                .unwrap_or(false)
            {
                return rt.complete_agent_llm(self.actor_id, prompt);
            }
            let request = rt.build_actor_llm_request(self.actor_id, model, prompt)?;
            let module = rt.actors.get(&self.actor_id)?.bytecode_module.clone()?;
            rt.complete_llm_with_tools(request, Vec::new(), &module)
                .ok()?
                .content
        }
    }

    #[cfg(feature = "ai-runtime")]
    fn llm_ask(&mut self, model: &str, prompt: &str) -> crate::vm::PerformAsyncResult {
        use crate::vm::PerformAsyncResult;
        unsafe {
            let rt = &mut *self.runtime;
            let actor_id = self.actor_id;

            // Nested synchronous paths (pipelines, ask_actor_sync) keep the
            // blocking behavior.
            if !rt.suspend_enabled {
                return PerformAsyncResult::Ready(self.complete_llm(model, prompt));
            }

            // Re-executed after a resume: a completed response is waiting.
            let completed = rt
                .actors
                .get_mut(&actor_id)
                .and_then(|actor| actor.llm_completed.take());
            if let Some(result) = completed {
                return match result {
                    Ok(response) => {
                        // Finish on the scheduler thread: tool invocation and
                        // durable-state write-back must not run on the worker.
                        let prev_current_actor = rt.current_actor;
                        rt.current_actor = Some(actor_id);
                        let is_agent = rt
                            .actors
                            .get(&actor_id)
                            .map(|a| a.is_agent)
                            .unwrap_or(false);
                        let content = if is_agent {
                            let module = rt
                                .actors
                                .get(&actor_id)
                                .and_then(|a| a.bytecode_module.clone());
                            let processed = match module {
                                Some(m) => rt.finish_tool_calls(&m, response),
                                None => Ok(response),
                            };
                            match processed {
                                Ok(resp) => agent::finish_agent_llm(rt, actor_id, prompt, &resp),
                                Err(_) => None,
                            }
                        } else {
                            let module = rt
                                .actors
                                .get(&actor_id)
                                .and_then(|a| a.bytecode_module.clone());
                            match module {
                                Some(m) => rt
                                    .finish_tool_calls(&m, response)
                                    .ok()
                                    .and_then(|r| r.content),
                                None => response.content,
                            }
                        };
                        rt.current_actor = prev_current_actor;
                        PerformAsyncResult::Ready(content)
                    }
                    Err(_) => PerformAsyncResult::Ready(None),
                };
            }

            // A call is already in flight (defensive; should not happen).
            if rt
                .actors
                .get(&actor_id)
                .map(|a| a.llm_inflight)
                .unwrap_or(false)
            {
                return PerformAsyncResult::Pending;
            }

            // Authority must still be valid immediately before the first
            // externally-observable provider request. Completed/resumed calls
            // above do not re-authorize an operation that already crossed the
            // boundary.
            if authorize_actor_host_effect(
                rt,
                (actor_id != 0).then_some(actor_id),
                "Inference",
                Some("ask"),
                &[],
                &[],
            )
            .is_err()
            {
                return PerformAsyncResult::Ready(None);
            }

            // Build the request on the scheduler thread, then hand it to a
            // background worker for the HTTP call.
            let is_agent = rt
                .actors
                .get(&actor_id)
                .map(|a| a.is_agent)
                .unwrap_or(false);
            let request = if is_agent {
                agent::build_agent_llm_request(rt, actor_id, prompt)
            } else {
                rt.build_actor_llm_request(actor_id, model, prompt)
            };
            // Build failure (e.g. missing agent state fields): nil response.
            let Some(request) = request else {
                return PerformAsyncResult::Ready(None);
            };
            if !(*rt).dispatch_llm_request(actor_id, request, prompt) {
                // Dispatch failed: fall back to a nil response.
                rt.llm.inflight_count = rt.llm.inflight_count.saturating_sub(1);
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    actor.llm_inflight = false;
                    actor.llm_pending_prompt = None;
                }
                return PerformAsyncResult::Ready(None);
            }
            PerformAsyncResult::Pending
        }
    }

    #[cfg_attr(not(feature = "ai-runtime"), allow(unused_variables))]
    fn perform_async(
        &mut self,
        effect_op: &str,
        constants: &[crate::bytecode::Constant],
        args: &[crate::vm::Value],
    ) -> crate::vm::PerformAsyncResult {
        use crate::vm::PerformAsyncResult;
        match effect_op {
            #[cfg(feature = "ai-runtime")]
            "Inference.ask" | "LLM.ask" => {
                let prompt = resolve_first_string(constants, args);
                self.llm_ask("", &prompt)
            }
            "Timer.sleep" => {
                let ms = args.first().and_then(|v| v.as_int()).unwrap_or(0) as u64;
                unsafe {
                    let rt = &mut *self.runtime;
                    if let Some(actor) = rt.actors.get_mut(&self.actor_id) {
                        if actor.timer_sleep_fired {
                            actor.timer_sleep_fired = false;
                            return PerformAsyncResult::Ready(None);
                        }
                    }
                    if ms == 0 {
                        return PerformAsyncResult::Ready(None);
                    }
                    if ms > 0 {
                        rt.timer_wheel
                            .timer_sleep_wake(std::time::Duration::from_millis(ms), self.actor_id);
                    }
                }
                PerformAsyncResult::Pending
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.new" => {
                let id = unsafe { (*self.runtime).pipeline_new() };
                PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.stage" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let actor = actor_arg(args, 2);
                let template = string_arg(constants, args, 3);
                let result = unsafe { (*self.runtime).pipeline_stage(id, &name, actor, &template) };
                let r = result.map(|id| id as i64).unwrap_or(-1);
                PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Pipeline.run" => {
                let id = id_arg(constants, args, 0);
                let input = string_arg(constants, args, 1);
                let result = unsafe { (*self.runtime).pipeline_run(id, &input).ok() };
                PerformAsyncResult::Ready(result)
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.new" => {
                let id = unsafe { (*self.runtime).supervisor_new() };
                PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.worker" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let actor = actor_arg(args, 2);
                let description = string_arg(constants, args, 3);
                let result =
                    unsafe { (*self.runtime).supervisor_worker(id, &name, actor, &description) };
                let r = result.map(|id| id as i64).unwrap_or(-1);
                PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Supervisor.run" => {
                let id = id_arg(constants, args, 0);
                let task = string_arg(constants, args, 1);
                let result = unsafe { (*self.runtime).supervisor_run(id, &task).ok() };
                PerformAsyncResult::Ready(result)
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.new" => {
                let topic = string_arg(constants, args, 0);
                let rounds = int_arg(args, 1);
                let threshold = float_arg(args, 2);
                let id = unsafe { (*self.runtime).debate_new(&topic, rounds, threshold) };
                PerformAsyncResult::Ready(Some(id.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.participant" => {
                let id = id_arg(constants, args, 0);
                let name = string_arg(constants, args, 1);
                let stance = string_arg(constants, args, 2);
                let actor = actor_arg(args, 3);
                let result =
                    unsafe { (*self.runtime).debate_participant(id, &name, &stance, actor) };
                let r = result.map(|id| id as i64).unwrap_or(-1);
                PerformAsyncResult::Ready(Some(r.to_string()))
            }
            #[cfg(feature = "ai-runtime")]
            "Debate.run" => {
                let id = id_arg(constants, args, 0);
                let result = unsafe { (*self.runtime).debate_run(id).ok() };
                PerformAsyncResult::Ready(result)
            }
            _ => PerformAsyncResult::Ready(None),
        }
    }

    fn try_receive(&mut self) -> Option<(u16, crate::vm::Value)> {
        unsafe {
            let msg = {
                let actor = (*self.runtime).actors.get_mut(&self.actor_id)?;
                actor.mailbox.pop()?
            };
            // ORCA receiver protocol: hold heap pointers carried by the message.
            (*self.runtime).hold_payload_refs(self.actor_id, &*msg.payload);
            let val = msg
                .payload
                .first()
                .cloned()
                .unwrap_or(crate::vm::Value::unit());
            Some((msg.behavior_id, val))
        }
    }

    fn try_receive_match(
        &mut self,
        behavior_ids: &[u16],
    ) -> Option<(usize, Vec<crate::vm::Value>)> {
        unsafe {
            let (pos, payload) = {
                let actor = (*self.runtime).actors.get_mut(&self.actor_id)?;
                actor.mailbox.receive_match(behavior_ids)?
            };
            // ORCA receiver protocol: hold heap pointers carried by the message.
            (*self.runtime).hold_payload_refs(self.actor_id, &*payload);
            Some((
                pos,
                Arc::try_unwrap(payload).unwrap_or_else(|arc| (*arc).clone()),
            ))
        }
    }

    fn receive_wait_suspend(&mut self, timeout_ms: i64) -> bool {
        unsafe {
            let rt = &mut *self.runtime;
            let Some(actor) = rt.actors.get_mut(&self.actor_id) else {
                return false;
            };
            // A fired timeout resolves the wait exactly once: consume the
            // marker so the re-executed ReceiveWait writes the no-match
            // sentinel and a later wait starts clean.
            if actor.receive_wait.map(|w| w.timed_out).unwrap_or(false) {
                actor.receive_wait = None;
                return false;
            }
            // Non-positive timeouts poll once (Erlang-style non-blocking
            // receive). Synchronous entry points (ask_actor_sync: pipelines,
            // supervisors, debates, `Ask`) never suspend — same gating as
            // the non-blocking LLM path.
            if timeout_ms <= 0 || !rt.suspend_enabled {
                return false;
            }
            true
        }
    }

    fn receive_wait_matched(&mut self) {
        unsafe {
            let rt = &mut *self.runtime;
            let wait = rt
                .actors
                .get_mut(&self.actor_id)
                .and_then(|a| a.receive_wait.take());
            // A match resolves the wait: cancel the pending timeout so it
            // cannot fire into a later wait on this actor.
            if let Some(wait) = wait {
                rt.timer_wheel.cancel(wait.timer_id);
            }
        }
    }

    fn commit_receive_match(&mut self) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.mailbox.commit_receive_match();
            }
        }
    }

    fn reset_receive_match(&mut self) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.mailbox.reset_receive_match();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Distributed callbacks for the bytecode VM — bridges RSend/RAsk/RSpawn
// opcodes to the runtime's send_distributed infrastructure.
// ---------------------------------------------------------------------------

/// Raw-pointer callbacks for distributed VM opcodes (`RSend`, `RAsk`,
/// `Migrate`, `RSpawn`, `Gossip`).  Mirrors [`BytecodeRuntimeCallbacks`]
/// in using a transient `*mut Runtime` borrow — the VM calls these only
/// while the runtime holds `&mut self`, so the pointer is valid and unique.
#[derive(Debug)]
pub(crate) struct BytecodeDistributedCallbacks {
    pub(crate) runtime: *mut Runtime,
}

// SAFETY: the VM only invokes these callbacks while the calling
// `Runtime` method holds `&mut self`.  The raw pointer is therefore the
// sole active borrow of the runtime.
unsafe impl Send for BytecodeDistributedCallbacks {}
unsafe impl Sync for BytecodeDistributedCallbacks {}

impl crate::vm::DistributedVmCallbacks for BytecodeDistributedCallbacks {
    fn node_id(&self) -> u64 {
        unsafe {
            (*self.runtime)
                .distributed
                .node_id
                .map(|n| n.0)
                .unwrap_or(0)
        }
    }

    fn remote_send(
        &mut self,
        target_actor: u64,
        target_node: u64,
        behavior: &str,
        args: &[crate::vm::Value],
    ) {
        unsafe {
            let rt = &mut *self.runtime;
            let node_id = rt.distributed.node_id.map(|n| n.0).unwrap_or(0);
            // If the target is the local node, or distributed transport is
            // not available, fall back to local delivery instead of silently
            // dropping the message.
            if target_node == node_id
                || rt.distributed.transport.is_none()
                || rt.distributed.cluster.is_none()
                || rt.distributed.resolver.is_none()
            {
                rt.send_message(target_actor, behavior, args);
                return;
            }
            // Take distributed fields out so send_distributed can borrow
            // them independently of rt itself.
            let mut transport = rt.distributed.transport.take();
            let mut resolver = rt.distributed.resolver.take();
            let cluster = rt.distributed.cluster.take();
            if let (Some(ref mut t), Some(ref c), Some(ref mut r)) =
                (&mut transport, &cluster, &mut resolver)
            {
                let target = ActorAddress::remote(NodeId(target_node), target_actor);
                send_distributed(rt, t, c, r, target, behavior, args);
            }
            rt.distributed.transport = transport;
            rt.distributed.resolver = resolver;
            rt.distributed.cluster = cluster;
        }
    }

    fn migrate(&mut self, actor_id: u64, target_node_id: u64) {
        unsafe {
            let rt = &mut *self.runtime;
            let target = NodeId(target_node_id);

            // Extract all needed data from the actor in a tight scope so the
            // immutable borrow on rt.actors is released before reap_living_actor
            // takes a mutable borrow on rt.
            let (snapshot_json, nbc_bytes) = {
                let actor = match rt.actors.get(&actor_id) {
                    Some(a) => a,
                    None => {
                        tracing::warn!(
                            "nulang-migrate: actor {} not found for migration to {:?}",
                            actor_id,
                            target
                        );
                        return;
                    }
                };

                // Build the durable-state snapshot.
                let mut state = std::collections::HashMap::new();
                for (name, value) in &actor.state_data {
                    let model = actor
                        .state_models
                        .get(name)
                        .copied()
                        .unwrap_or(crate::runtime::persistence::StateModel::Local);
                    if model == crate::runtime::persistence::StateModel::Durable || model.is_crdt()
                    {
                        let persisted = if name == "semantic_memory" || name == "procedural_memory"
                        {
                            crate::runtime::workflow::vm_value_to_string_in_actor(
                                    value, actor,
                                )
                                .map(crate::runtime::persistence::PersistedValue::String)
                                .unwrap_or_else(|| {
                                    crate::runtime::persistence::PersistedValue::from_value_resolved(
                                        value,
                                        actor.bytecode_module.as_ref(),
                                    )
                                })
                        } else {
                            crate::runtime::persistence::PersistedValue::from_value_resolved(
                                value,
                                actor.bytecode_module.as_ref(),
                            )
                        };
                        state.insert(name.clone(), persisted);
                    }
                }

                // Snapshot global CRDT state.
                let crdt_snapshot = rt.crdt_manager.as_ref().map(|m| {
                    m.snapshot()
                        .into_iter()
                        .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
                        .collect()
                });

                let crdt_field_map = rt.crdt_manager.as_ref().map(|m| {
                    m.field_map
                        .iter()
                        .filter(|((aid, _), _)| *aid == actor_id)
                        .map(|((_, name), id)| (name.clone(), id.0))
                        .collect()
                });
                let snapshot = crate::runtime::persistence::ActorSnapshot {
                    actor_id,
                    sequence: actor.sequence,
                    state,
                    waiting_signal: actor.waiting_signal.clone(),
                    crdt_snapshot,
                    crdt_field_map,
                authority_tokens: Default::default(),
                };

                let snapshot_json = match serde_json::to_vec(&snapshot) {
                    Ok(json) => json,
                    Err(e) => {
                        tracing::warn!(
                            "nulang-migrate: failed to serialize snapshot for actor {}: {}",
                            actor_id,
                            e
                        );
                        return;
                    }
                };

                // Get NBC-encoded bytecode module.
                let module = match actor.bytecode_module.as_ref() {
                    Some(m) => m.clone(),
                    None => match rt.recovery_modules.get(&actor_id) {
                        Some((m, _, _)) => m.clone(),
                        None => {
                            tracing::warn!(
                                "nulang-migrate: no bytecode module for actor {}",
                                actor_id
                            );
                            return;
                        }
                    },
                };
                let nbc = match module.to_nbc(None) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            "nulang-migrate: failed to encode NBC for actor {}: {}",
                            actor_id,
                            e
                        );
                        return;
                    }
                };

                (snapshot_json, nbc)
            }; // <- actor borrow released here

            // Send the migration packet.
            let target_addr = rt
                .distributed
                .cluster
                .as_ref()
                .and_then(|c| c.get_node(target))
                .map(|info| info.address);

            let packet = super::network::Packet::MigrateActor {
                actor_id,
                nbc_bytes,
                snapshot_json,
            };

            if let (Some(transport), Some(addr)) = (&mut rt.distributed.transport, target_addr) {
                transport.send(target, addr, packet);
            } else {
                tracing::warn!(
                    "nulang-migrate: cannot reach target node {:?} for actor {}",
                    target,
                    actor_id
                );
                return;
            }

            // Register forwarding entry BEFORE reaping.
            rt.migrated_actors
                .insert(actor_id, (target, std::time::Instant::now()));

            // Reap the actor cleanly (no supervisor restart — migration is
            // intentional relocation, not a crash).
            crate::runtime::exit::reap_living_actor(rt, actor_id, crate::types::ExitReason::Normal);

            tracing::info!(
                "nulang-migrate: actor {} migrated to node {:?}",
                actor_id,
                target
            );
        }
    }
    fn remote_ask(
        &mut self,
        target_actor: u64,
        behavior: &str,
        args: &[crate::vm::Value],
        _timeout_ms: u64,
    ) -> crate::vm::Value {
        // Send the ask request over the network. The reply is expected to
        // arrive via the normal message path (the target actor sends back
        // a response message). The caller should use `receive` to collect
        // the reply. Full suspend/resume support (RFC 0007) would block
        // the actor until the reply or timeout.
        unsafe {
            let rt = &mut *self.runtime;
            // Cross-node routing by bare actor-ref value: if the target id
            // is a known remote ref (spawn@node placeholder or inbound
            // sender), route to ITS node; otherwise fall back to the
            // local-node path (single-node `ask remote` local delivery).
            match rt.remote_refs.get(&target_actor).copied() {
                Some(node) => {
                    rt.route_ref_send(target_actor, node, behavior, args);
                }
                None => {
                    let node_id = rt.distributed.node_id.map(|n| n.0).unwrap_or(0);
                    let target =
                        ActorAddress::remote(crate::runtime::NodeId(node_id), target_actor);
                    rt.send_distributed(target, behavior, args);
                }
            }
        }
        crate::vm::Value::nil()
    }
    fn remote_spawn(
        &mut self,
        target_node: u64,
        behavior: &str,
        init: &[(String, crate::vm::Value)],
    ) -> crate::vm::Value {
        unsafe {
            let rt = &mut *self.runtime;
            let node = NodeId(target_node);
            let mut transport = rt.distributed.transport.take();
            let mut resolver = rt.distributed.resolver.take();
            let cluster = rt.distributed.cluster.take();
            let result = if let (Some(ref mut t), Some(ref c), Some(ref mut r)) =
                (&mut transport, &cluster, &mut resolver)
            {
                let addr = spawn_on_node(rt, t, c, r, node, behavior, init.to_vec());
                crate::vm::Value::actor_ref(addr.actor_id())
            } else {
                crate::vm::Value::actor_ref(0)
            };
            rt.distributed.transport = transport;
            rt.distributed.resolver = resolver;
            rt.distributed.cluster = cluster;
            result
        }
    }
    fn gossip(&mut self, _message: &str) -> crate::vm::Value {
        crate::vm::Value::unit()
    }
}


#[cfg(test)]
mod host_authority_tests {
    use super::{authorize_actor_host_effect, http_outbound_authority, required_host_authority};
    use crate::authority::{AuthorityGrant, AuthorityManifest};
    use crate::bytecode::Constant;
    use crate::runtime::{Actor, Runtime};
    use crate::vm::Value;

    fn string_args(values: &[&str]) -> (Vec<Constant>, Vec<Value>) {
        let constants: Vec<_> = values
            .iter()
            .map(|value| Constant::String((*value).to_string()))
            .collect();
        let regs = (0..values.len()).map(|idx| Value::string(idx as u32)).collect();
        (constants, regs)
    }

    #[test]
    fn http_authority_uses_exact_host_and_effective_port() {
        assert_eq!(
            http_outbound_authority("https://Api.Example.com/path").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "api.example.com".into(),
                port: 443,
            }
        );
        assert_eq!(
            http_outbound_authority("http://example.com:8080?q=1").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "example.com".into(),
                port: 8080,
            }
        );
        assert_eq!(
            http_outbound_authority("https://[2001:db8::1]/").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "[2001:db8::1]".into(),
                port: 443,
            }
        );
    }

    #[test]
    fn malformed_or_userinfo_http_targets_fail_closed() {
        for url in [
            "ftp://example.com/x",
            "https://user@example.com/x",
            "https://example.com:0/x",
            "https://2001:db8::1/x",
            "https:///x",
        ] {
            assert!(http_outbound_authority(url).is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn host_effects_map_to_typed_exact_grants() {
        let (constants, regs) = string_args(&["/tmp/input.txt"]);
        assert_eq!(
            required_host_authority("FS", Some("read"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::FsRead {
                path: "/tmp/input.txt".into(),
            })
        );

        let (constants, regs) = string_args(&["HOME"]);
        assert_eq!(
            required_host_authority("Env", Some("get"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::EnvRead { name: "HOME".into() })
        );

        let (constants, regs) = string_args(&["PAYMENTS_KEY"]);
        assert_eq!(
            required_host_authority("Secret", Some("read"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::SecretRead {
                name: "PAYMENTS_KEY".into(),
            })
        );
    }

    #[test]
    fn web_static_and_realtime_broadcast_map_to_exact_authority() {
        let (constants, regs) = string_args(&["/srv/public/index.html"]);
        assert_eq!(
            required_host_authority("Web", Some("serve_static"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::FsRead {
                path: "/srv/public/index.html".into(),
            })
        );

        let (constants, regs) = string_args(&["orders", "changed"]);
        assert_eq!(
            required_host_authority("Realtime", Some("broadcast"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::Other {
                namespace: "Realtime".into(),
                operation: "Broadcast".into(),
                argument: Some("orders".into()),
            })
        );
    }

    #[test]
    fn runtime_callback_blocks_web_serve_static_without_exact_file_grant() {
        use crate::vm::ActorVmCallbacks;
        use std::cell::RefCell;
        use std::rc::Rc;

        let unique = format!(
            "nulang-authority-static-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let file = std::env::temp_dir().join(unique);
        std::fs::write(&file, "authority-protected-static-content").unwrap();
        let file_string = file.to_string_lossy().into_owned();

        let runtime = Rc::new(RefCell::new(Runtime::new()));
        let actor_id = 800_006;
        {
            let mut rt = runtime.borrow_mut();
            rt.actors
                .insert(actor_id, Actor::new(actor_id, "web-static-authority", 8));
            rt.current_actor = Some(actor_id);
        }

        let module = {
            let mut module = crate::bytecode::CodeModule::new("web-static-authority");
            module.add_constant(Constant::String(file_string.clone()));
            module
        };
        let regs = [Value::string(0)];
        let mut callbacks = super::RuntimeVmCallbacks::new(runtime.clone());

        let denied = callbacks
            .perform_builtin_effect_in_module("Web", Some("serve_static"), &module, &regs)
            .expect("denied Web.serve_static must be handled");
        assert!(denied.is_nil(), "unprivileged actor must not read static file");

        {
            let mut rt = runtime.borrow_mut();
            let token = format!("Fs::Read({file_string})");
            let manifest = AuthorityManifest::from_tokens([token.as_str()]).unwrap();
            rt.actors
                .get_mut(&actor_id)
                .unwrap()
                .install_authority_manifest(&manifest);
        }

        let allowed = callbacks
            .perform_builtin_effect_in_module("Web", Some("serve_static"), &module, &regs)
            .expect("authorized Web.serve_static must be handled");
        assert_eq!(
            crate::vm::resolve_value_string(&[], allowed),
            "authority-protected-static-content"
        );

        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn actor_host_access_is_deny_by_default_and_exact_match_only() {
        let mut rt = Runtime::new();
        let actor_id = 800_001;
        rt.actors.insert(actor_id, Actor::new(actor_id, "host-authority", 8));
        let (constants, regs) = string_args(&["/tmp/allowed.txt"]);

        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_err());

        let manifest = AuthorityManifest::from_tokens(["Fs::Read(/tmp/allowed.txt)"]).unwrap();
        rt.actors
            .get_mut(&actor_id)
            .unwrap()
            .install_authority_manifest(&manifest);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_ok());

        let (other_constants, other_regs) = string_args(&["/tmp/other.txt"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &other_constants,
            &other_regs,
        )
        .is_err());
    }

    #[test]
    fn malformed_actor_manifest_cannot_authorize_exact_present_grant() {
        let mut rt = Runtime::new();
        let actor_id = 800_002;
        let mut actor = Actor::new(actor_id, "host-authority-invalid", 8);
        actor.capabilities.insert("Env::Read(HOME)".to_string());
        actor
            .capabilities
            .insert("Net::TcpOut(malformed)".to_string());
        rt.actors.insert(actor_id, actor);
        let (constants, regs) = string_args(&["HOME"]);

        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Env",
            Some("get"),
            &constants,
            &regs,
        )
        .is_err());
    }

    #[test]
    fn actor_http_authority_is_exact_destination_not_category_permission() {
        let mut rt = Runtime::new();
        let actor_id = 800_003;
        let mut actor = Actor::new(actor_id, "host-network-authority", 8);
        let manifest = AuthorityManifest::from_tokens(["Net::TcpOut(api.example.com:443)"]).unwrap();
        actor.install_authority_manifest(&manifest);
        rt.actors.insert(actor_id, actor);

        let (constants, regs) = string_args(&["https://api.example.com/v1"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Http",
            Some("get"),
            &constants,
            &regs,
        )
        .is_ok());

        let (constants, regs) = string_args(&["https://api.example.com:8443/v1"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Http",
            Some("get"),
            &constants,
            &regs,
        )
        .is_err());
    }

    #[test]
    fn actor_ffi_authority_is_exact_library_and_symbol() {
        let mut rt = Runtime::new();
        let actor_id = 800_004;
        let mut actor = Actor::new(actor_id, "host-ffi-authority", 8);
        let manifest = AuthorityManifest::from_tokens([
            "FFI::Call(libpayments.so::charge)"
        ])
        .unwrap();
        actor.install_authority_manifest(&manifest);
        rt.actors.insert(actor_id, actor);

        assert!(super::authorize_actor_ffi(
            &rt,
            Some(actor_id),
            "libpayments.so",
            "charge",
        )
        .is_ok());
        assert!(super::authorize_actor_ffi(
            &rt,
            Some(actor_id),
            "libpayments.so",
            "refund",
        )
        .is_err());
        assert!(super::authorize_actor_ffi(
            &rt,
            Some(actor_id),
            "libother.so",
            "charge",
        )
        .is_err());
    }

    #[test]
    fn bytecode_top_level_zero_sentinel_is_actor_free_but_missing_real_actor_fails_closed() {
        let mut rt = Runtime::new();
        let top_level = super::BytecodeRuntimeCallbacks::new(&mut rt as *mut Runtime, 0);
        assert_eq!(top_level.authority_actor_id(), None);

        let missing_actor = super::BytecodeRuntimeCallbacks::new(&mut rt as *mut Runtime, 999_999);
        assert_eq!(missing_actor.authority_actor_id(), Some(999_999));

        let (constants, regs) = string_args(&["/tmp/sentinel.txt"]);
        assert!(authorize_actor_host_effect(
            &rt,
            top_level.authority_actor_id(),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_ok());
        assert!(authorize_actor_host_effect(
            &rt,
            missing_actor.authority_actor_id(),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_err());
    }

    #[test]
    fn runtime_callback_denies_http_serve_before_socket_bind() {
        use crate::vm::ActorVmCallbacks;
        use std::cell::RefCell;
        use std::rc::Rc;

        let runtime = Rc::new(RefCell::new(Runtime::new()));
        let actor_id = 800_005;
        {
            let mut rt = runtime.borrow_mut();
            rt.actors
                .insert(actor_id, Actor::new(actor_id, "host-http-denied", 8));
            rt.current_actor = Some(actor_id);
        }

        let mut callbacks = super::RuntimeVmCallbacks::new(runtime.clone());
        let module = crate::bytecode::CodeModule::new("host-http-denied");
        let result = callbacks.perform_builtin_effect_in_module(
            "Http",
            Some("serve"),
            &module,
            &[Value::int(0), Value::int(0)],
        );

        assert!(result.expect("denied host effect must be handled").is_nil());
        assert!(
            runtime.borrow().http_server.is_none(),
            "denied Http.serve must not bind a socket"
        );
    }

    #[test]
    fn actor_free_runtime_keeps_existing_ambient_host_contract() {
        let rt = Runtime::new();
        let (constants, regs) = string_args(&["/tmp/ambient.txt"]);
        assert!(authorize_actor_host_effect(
            &rt,
            None,
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_ok());
    }
}
