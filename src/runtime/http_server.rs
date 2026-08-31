//! HTTP/1.1 server built on std::net::TcpListener with httparse.
//!
//! Phase 1: each request dispatches to a standalone VM on the listener thread.
//! Handlers are non-capturing `fn(String) -> String` — no closures with env.
//! Status is always 200; Content-Type is always text/plain.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::bytecode::CodeModule;
use crate::value_layout::PAYLOAD_MASK;
use crate::vm::{resolve_value_string, Value, CLOSURE_ENV_FLAG, VM};
use crate::web::reactivity::inject_client_runtime_script;

/// HTTP method — must match the Nulang-level variant type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpMethod {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
        }
    }
}

#[allow(dead_code)] // Phase 2: method/path/headers will be passed to handlers
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Manages the background HTTP listener thread.
/// Stored on Runtime; `HttpServerState::bind()` spawns the thread.
pub struct HttpServerState {
    /// Listen port (the actual port, after bind — useful when port 0 is used).
    pub port: u16,
    /// Clone of the handler's module (for per-request VM creation).
    pub handler_module: CodeModule,
    /// Function table index of the handler function within handler_module.
    pub handler_func_idx: usize,
    /// True while the server is running; set to false to signal shutdown.
    shutdown_flag: Arc<AtomicBool>,
    /// Listener thread handle.
    listener_thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for HttpServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // JoinHandle is not Debug; print the fields that are.
        f.debug_struct("HttpServerState")
            .field("port", &self.port)
            .field("handler_func_idx", &self.handler_func_idx)
            .field("shutdown_flag", &self.shutdown_flag.load(Ordering::Relaxed))
            .finish()
    }
}

impl Drop for HttpServerState {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.listener_thread.take() {
            let _ = handle.join();
        }
    }
}

impl HttpServerState {
    const MAX_BODY_SIZE: usize = 1_048_576; // 1 MB

    pub fn bind(
        port: u16,
        handler_module: CodeModule,
        handler_func_idx: usize,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let actual_port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let module_clone = handler_module.clone();

        let handle = std::thread::Builder::new()
            .name("nulang-http-listener".into())
            .spawn(move || {
                Self::listener_loop(listener, module_clone, handler_func_idx, shutdown_clone);
            })?;

        Ok(HttpServerState {
            port: actual_port,
            handler_module,
            handler_func_idx,
            shutdown_flag: shutdown,
            listener_thread: Some(handle),
        })
    }

    fn listener_loop(
        listener: TcpListener,
        handler_module: CodeModule,
        handler_func_idx: usize,
        shutdown: Arc<AtomicBool>,
    ) {
        listener.set_nonblocking(true).ok();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    Self::handle_connection(stream, &handler_module, handler_func_idx);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        handler_module: &CodeModule,
        handler_func_idx: usize,
    ) {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let mut headers = [httparse::EMPTY_HEADER; 64];
                    let mut req = httparse::Request::new(&mut headers);
                    match req.parse(&buf[..n]) {
                        Ok(httparse::Status::Complete(body_offset)) => {
                            let method = HttpMethod::from_str(req.method.unwrap_or("GET"))
                                .unwrap_or(HttpMethod::Get);
                            let path = req.path.unwrap_or("/").to_string();

                            let headers: Vec<(String, String)> = req
                                .headers
                                .iter()
                                .map(|h| {
                                    (
                                        h.name.to_string(),
                                        String::from_utf8_lossy(h.value).to_string(),
                                    )
                                })
                                .collect();

                            let content_length: usize = headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, v)| v.parse().ok())
                                .unwrap_or(0);

                            let mut body = Vec::new();
                            if body_offset < n {
                                body.extend_from_slice(&buf[body_offset..n]);
                            }
                            while body.len() < content_length.min(Self::MAX_BODY_SIZE) {
                                let mut chunk = [0u8; 4096];
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(m) => body.extend_from_slice(&chunk[..m]),
                                    Err(_) => break,
                                }
                            }

                            if content_length > Self::MAX_BODY_SIZE {
                                Self::write_response(
                                    &mut stream,
                                    &HttpResponse {
                                        status: 413,
                                        headers: vec![("Content-Type".into(), "text/plain".into())],
                                        body: b"Payload too large".to_vec(),
                                    },
                                );
                                break;
                            }

                            let request = HttpRequest {
                                method,
                                path,
                                headers,
                                body,
                            };
                            let response =
                                Self::dispatch(handler_module, handler_func_idx, &request);
                            let keep_alive = false; // Phase 1: close after each request
                            Self::write_response(&mut stream, &response);
                            if !keep_alive {
                                break;
                            }
                        }
                        Ok(httparse::Status::Partial) => continue, // need more data
                        Err(_) => {
                            Self::write_response(
                                &mut stream,
                                &HttpResponse {
                                    status: 400,
                                    headers: vec![("Content-Type".into(), "text/plain".into())],
                                    body: b"Bad request".to_vec(),
                                },
                            );
                            break;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn write_response(stream: &mut TcpStream, response: &HttpResponse) {
        let status_text = match response.status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Unknown",
        };
        let mut out = format!("HTTP/1.1 {} {}\r\n", response.status, status_text);
        for (k, v) in &response.headers {
            out.push_str(&format!("{}: {}\r\n", k, v));
        }
        out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
        out.push_str("\r\n");
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.write_all(&response.body);
        let _ = stream.flush();
    }

    /// Dispatch a request to the handler via a fresh standalone VM.
    ///
    /// Clones the handler module, injects the request body as a string constant,
    /// emits trampoline bytecode that loads the body into r0, creates a closure
    /// for the handler function, calls it, and returns. The handler receives the
    /// body string in r0 and is expected to return a string in r0.
    fn dispatch(
        handler_module: &CodeModule,
        handler_func_idx: usize,
        request: &HttpRequest,
    ) -> HttpResponse {
        use crate::bytecode::{Constant, Instruction, OpCode};

        let mut vm = VM::new();
        let mut module = handler_module.clone();

        let body_str = String::from_utf8_lossy(&request.body).to_string();
        let body_idx = module.add_constant(Constant::String(body_str));

        let entry_offset = module.instructions.len();
        // ConstU body_idx -> r0 (first argument to handler)
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((body_idx >> 8) & 0xFF) as u8,
            (body_idx & 0xFF) as u8,
            0,
        ));
        // Closure handler_func_idx -> r1 (function reference)
        module.emit(Instruction::new3(
            OpCode::Closure,
            ((handler_func_idx >> 8) & 0xFF) as u8,
            (handler_func_idx & 0xFF) as u8,
            1,
        ));
        // ClosureCall r1, 0, r0 — call closure in r1, result -> r0
        module.emit(Instruction::new3(OpCode::ClosureCall, 1, 0, 0));
        // Ret — pops frame, returns r0 to VM
        module.emit(Instruction::new0(OpCode::Ret));

        vm.load_module(module);
        match vm.run_from(0, entry_offset) {
            Ok(result) => {
                let body = vm.value_to_string(0, result);
                HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: body.into_bytes(),
                }
            }
            Err(_) => HttpResponse {
                status: 500,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: b"Internal server error".to_vec(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Web route representation and request context.
// ---------------------------------------------------------------------------

/// Extract a stable function-table index from a value passed as a handler.
///
/// Top-level function references are emitted by the compiler as raw integers.
/// Non-capturing closures are also usable (their payload is the function
/// index). Capturing closures are rejected because their environment is not
/// stable across requests.
pub fn extract_function_index(value: Value) -> Option<usize> {
    if value.is_closure() {
        let payload = value.as_raw() & PAYLOAD_MASK;
        if payload & CLOSURE_ENV_FLAG != 0 {
            return None; // capturing closures cannot be stored as route handlers
        }
        Some(payload as usize)
    } else {
        value.as_int().map(|n| n as usize)
    }
}

/// A registered web route: method + path mapped to a handler function index.
#[derive(Clone, Debug)]
pub struct WebRoute {
    pub method: HttpMethod,
    pub path: String,
    pub handler_module: CodeModule,
    pub handler_func_idx: usize,
}

impl WebRoute {
    /// Build a route from the three register arguments staged by `perform Web.route`.
    pub fn from_registers(
        method_val: Value,
        path_val: Value,
        handler_val: Value,
        constants: &[crate::bytecode::Constant],
        module: CodeModule,
    ) -> Option<Self> {
        let method = HttpMethod::from_str(&resolve_value_string(constants, method_val))?;
        let path = resolve_value_string(constants, path_val);
        let func_idx = extract_function_index(handler_val)?;
        Some(WebRoute {
            method,
            path,
            handler_module: module,
            handler_func_idx: func_idx,
        })
    }
}

/// Per-request context passed to route handlers when running under the dev
/// server or SSR. Holds the HTTP request and any route parameters captured
/// from the path pattern.
#[derive(Clone, Debug)]
pub struct RequestContext {
    pub request: HttpRequest,
    pub params: HashMap<String, String>,
}

thread_local! {
    static CURRENT_REQUEST: RefCell<Option<RequestContext>> = const { RefCell::new(None) };
    static RESPONSE_COOKIES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Set the request context for the current thread, run `f`, then clear it.
pub fn with_request_context<F, R>(ctx: RequestContext, f: F) -> R
where
    F: FnOnce() -> R,
{
    CURRENT_REQUEST.with(|c| *c.borrow_mut() = Some(ctx));
    RESPONSE_COOKIES.with(|c| c.borrow_mut().clear());
    let result = f();
    CURRENT_REQUEST.with(|c| *c.borrow_mut() = None);
    result
}

/// Add a `Set-Cookie` header for the current response.
pub fn set_cookie(name: &str, value: &str) {
    let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Lax", name, value);
    RESPONSE_COOKIES.with(|c| c.borrow_mut().push(cookie));
}

/// Add a `Set-Cookie` header that clears the named cookie.
pub fn clear_cookie(name: &str) {
    let cookie = format!("{}=; Path=/; Max-Age=0; SameSite=Lax", name);
    RESPONSE_COOKIES.with(|c| c.borrow_mut().push(cookie));
}

/// Take all cookies queued for the current response and clear the queue.
pub fn take_response_cookies() -> Vec<String> {
    RESPONSE_COOKIES.with(|c| c.borrow_mut().drain(..).collect())
}

/// Look up a captured route parameter for the current request.
pub fn current_request_param(name: &str) -> Option<String> {
    CURRENT_REQUEST.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ctx| ctx.params.get(name).cloned())
    })
}

/// Look up a request header for the current request (case-insensitive).
pub fn current_request_header(name: &str) -> Option<String> {
    CURRENT_REQUEST.with(|c| {
        c.borrow().as_ref().and_then(|ctx| {
            ctx.request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        })
    })
}

/// Return the body of the current request.
pub fn current_request_body() -> Option<Vec<u8>> {
    CURRENT_REQUEST.with(|c| c.borrow().as_ref().map(|ctx| ctx.request.body.clone()))
}

/// Look up a cookie value from the `Cookie` request header by name.
pub fn current_request_cookie(name: &str) -> Option<String> {
    let cookie_header = current_request_header("Cookie")?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Return the HTTP method of the current request.
pub fn current_request_method() -> Option<String> {
    CURRENT_REQUEST.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| ctx.request.method.as_str().to_string())
    })
}

/// Parse a URL-encoded form body into (name, value) pairs.
pub fn parse_form_urlencoded(body: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(body);
    let mut pairs = Vec::new();
    for part in s.split('&') {
        if part.is_empty() {
            continue;
        }
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("").to_string();
        let value = kv.next().unwrap_or("").to_string();
        pairs.push((percent_decode(&key), percent_decode(&value)));
    }
    pairs
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h1), Some(h2)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(((h1 << 4) | h2) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// In-memory key/value store for the web framework.
// ---------------------------------------------------------------------------

static WEB_KV_STORE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn web_kv_store() -> &'static Mutex<HashMap<String, String>> {
    WEB_KV_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn form_value(name: &str) -> Option<String> {
    current_request_body()
        .map(|b| parse_form_urlencoded(&b))
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

pub fn kv_get(key: &str) -> Option<String> {
    web_kv_store().lock().unwrap().get(key).cloned()
}

pub fn kv_set(key: &str, value: &str) {
    web_kv_store()
        .lock()
        .unwrap()
        .insert(key.to_string(), value.to_string());
}

pub fn kv_delete(key: &str) {
    web_kv_store().lock().unwrap().remove(key);
}

pub fn kv_all() -> Vec<(String, String)> {
    web_kv_store()
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Realtime broadcast registry for SSE (Server-Sent Events).
// ---------------------------------------------------------------------------

static REALTIME_BROADCAST: OnceLock<Mutex<HashMap<String, BroadcastRoom>>> = OnceLock::new();

fn realtime_broadcast_registry() -> &'static Mutex<HashMap<String, BroadcastRoom>> {
    REALTIME_BROADCAST.get_or_init(|| Mutex::new(HashMap::new()))
}

struct BroadcastRoom {
    messages: Vec<String>,
    listeners: Vec<Sender<String>>,
}

/// Broadcast a message to every subscriber of a room and keep it in the
/// room's replay history.
pub fn realtime_broadcast(room: &str, message: &str) {
    let mut registry = realtime_broadcast_registry().lock().unwrap();
    let entry = registry.entry(room.to_string()).or_insert(BroadcastRoom {
        messages: Vec::new(),
        listeners: Vec::new(),
    });
    entry.messages.push(message.to_string());

    let mut keep = Vec::new();
    for sender in entry.listeners.drain(..) {
        if sender.send(message.to_string()).is_ok() {
            keep.push(sender);
        }
    }
    entry.listeners = keep;
}

/// Subscribe to a room, returning the replay history and a receiver for
/// future messages.
pub fn realtime_subscribe(room: &str) -> (Vec<String>, Receiver<String>) {
    let (tx, rx) = channel::<String>();
    let mut registry = realtime_broadcast_registry().lock().unwrap();
    let entry = registry.entry(room.to_string()).or_insert(BroadcastRoom {
        messages: Vec::new(),
        listeners: Vec::new(),
    });
    let history = entry.messages.clone();
    entry.listeners.push(tx);
    (history, rx)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// A middleware that can transform an HTTP response or log a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Middleware {
    SecurityHeaders,
    RequestLog,
    #[allow(dead_code)]
    Csrf,
}

impl Middleware {
    /// Apply this middleware to a response for a given request.
    pub fn apply(&self, request: &HttpRequest, response: &mut HttpResponse) {
        match self {
            Middleware::SecurityHeaders => {
                let headers = &mut response.headers;
                headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
                headers.push(("X-Frame-Options".into(), "DENY".into()));
                headers.push((
                    "Referrer-Policy".into(),
                    "strict-origin-when-cross-origin".into(),
                ));
                headers.push((
                    "Content-Security-Policy".into(),
                    "default-src 'self'".into(),
                ));
            }
            Middleware::RequestLog => {
                eprintln!(
                    "{} {} -> {}",
                    request.method.as_str(),
                    request.path,
                    response.status
                );
            }
            Middleware::Csrf => {
                // Phase 2: generate CSRF token cookie and validate on POST.
            }
        }
    }
}

/// Match a route pattern against a request path, returning captured parameters.
/// Patterns use `:name` segments, e.g. `/products/:id`.
pub fn match_route(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pattern = pattern.trim_start_matches('/');
    let path = path.trim_start_matches('/');
    let pattern_parts: Vec<&str> = if pattern.is_empty() {
        vec![""]
    } else {
        pattern.split('/').collect()
    };
    let path_parts: Vec<&str> = if path.is_empty() {
        vec![""]
    } else {
        path.split('/').collect()
    };
    if pattern_parts.len() != path_parts.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (pat, seg) in pattern_parts.iter().zip(path_parts.iter()) {
        if pat.starts_with(':') {
            let name = &pat[1..];
            params.insert(name.to_string(), seg.to_string());
        } else if *pat != *seg {
            return None;
        }
    }
    Some(params)
}

/// Render a static route by calling its handler with no arguments.
/// Returns the resulting HTML string, or None if execution fails.
pub fn render_route_handler(
    module: &CodeModule,
    func_idx: usize,
    ctx: Option<RequestContext>,
) -> Option<String> {
    use crate::bytecode::{Instruction, OpCode};
    let mut vm = VM::new();
    let mut module = module.clone();
    let entry_offset = module.instructions.len();
    module.emit(Instruction::new3(
        OpCode::Closure,
        ((func_idx >> 8) & 0xFF) as u8,
        (func_idx & 0xFF) as u8,
        0,
    ));
    module.emit(Instruction::new3(OpCode::ClosureCall, 0, 0, 0));
    module.emit(Instruction::new0(OpCode::Ret));
    vm.load_module(module);
    let mut run = || match vm.run_from(0, entry_offset) {
        Ok(result) => Some(vm.value_to_string(0, result)),
        Err(_) => None,
    };
    if let Some(ctx) = ctx {
        with_request_context(ctx, run)
    } else {
        run()
    }
}

/// Dev server that dispatches registered `Web.route` handlers and falls back
/// to static files for unmatched paths.
#[derive(Debug)]
pub struct WebDevServer {
    pub port: u16,
    shutdown_flag: Arc<AtomicBool>,
    listener_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WebDevServer {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.listener_thread.take() {
            let _ = handle.join();
        }
    }
}

impl WebDevServer {
    const MAX_BODY_SIZE: usize = 1_048_576; // 1 MB
    const SSE_PATH_PREFIX: &'static str = "/__nulang/sse/";

    pub fn bind(
        port: u16,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<WebRoute>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let actual_port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let routes_clone = routes.clone();
        let static_dir_clone = static_dir.clone();
        let output_dir_clone = output_dir.clone();

        let handle = std::thread::Builder::new()
            .name("nulang-web-dev-listener".into())
            .spawn(move || {
                Self::listener_loop(
                    listener,
                    static_dir_clone,
                    output_dir_clone,
                    routes_clone,
                    shutdown_clone,
                );
            })?;

        Ok(WebDevServer {
            port: actual_port,
            shutdown_flag: shutdown,
            listener_thread: Some(handle),
        })
    }

    fn listener_loop(
        listener: TcpListener,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<WebRoute>,
        shutdown: Arc<AtomicBool>,
    ) {
        listener.set_nonblocking(true).ok();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let static_dir = static_dir.clone();
                    let output_dir = output_dir.clone();
                    let routes = routes.clone();
                    let shutdown = shutdown.clone();
                    std::thread::spawn(move || {
                        Self::handle_connection(stream, static_dir, output_dir, routes, shutdown);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<WebRoute>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut headers = [httparse::EMPTY_HEADER; 64];
                    let mut req = httparse::Request::new(&mut headers);
                    match req.parse(&buf[..n]) {
                        Ok(httparse::Status::Complete(body_offset)) => {
                            let method = HttpMethod::from_str(req.method.unwrap_or("GET"))
                                .unwrap_or(HttpMethod::Get);
                            let path = req.path.unwrap_or("/").to_string();

                            let headers: Vec<(String, String)> = req
                                .headers
                                .iter()
                                .map(|h| {
                                    (
                                        h.name.to_string(),
                                        String::from_utf8_lossy(h.value).to_string(),
                                    )
                                })
                                .collect();

                            let content_length: usize = headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, v)| v.parse().ok())
                                .unwrap_or(0);

                            let mut body = Vec::new();
                            if body_offset < n {
                                body.extend_from_slice(&buf[body_offset..n]);
                            }
                            while body.len() < content_length.min(Self::MAX_BODY_SIZE) {
                                let mut chunk = [0u8; 4096];
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(m) => body.extend_from_slice(&chunk[..m]),
                                    Err(_) => break,
                                }
                            }

                            if content_length > Self::MAX_BODY_SIZE {
                                HttpServerState::write_response(
                                    &mut stream,
                                    &HttpResponse {
                                        status: 413,
                                        headers: vec![("Content-Type".into(), "text/plain".into())],
                                        body: b"Payload too large".to_vec(),
                                    },
                                );
                                break;
                            }

                            let request = HttpRequest {
                                method,
                                path: path.clone(),
                                headers,
                                body,
                            };

                            let mut response = if path.starts_with(Self::SSE_PATH_PREFIX) {
                                let room = &path[Self::SSE_PATH_PREFIX.len()..];
                                Self::serve_sse(room, &mut stream, shutdown.clone());
                                break;
                            } else if let Some((route, params)) = routes
                                .iter()
                                .filter(|r| r.method == request.method)
                                .find_map(|r| match_route(&r.path, &request.path).map(|p| (r, p)))
                            {
                                let ctx = RequestContext {
                                    request: request.clone(),
                                    params,
                                };
                                match render_route_handler(
                                    &route.handler_module,
                                    route.handler_func_idx,
                                    Some(ctx),
                                ) {
                                    Some(html) => HttpResponse {
                                        status: 200,
                                        headers: vec![(
                                            "Content-Type".into(),
                                            "text/html; charset=utf-8".into(),
                                        )],
                                        body: inject_client_runtime_script(&html).into_bytes(),
                                    },
                                    None => HttpResponse {
                                        status: 500,
                                        headers: vec![("Content-Type".into(), "text/plain".into())],
                                        body: b"Internal server error".to_vec(),
                                    },
                                }
                            } else {
                                Self::serve_static(
                                    static_dir.as_deref(),
                                    output_dir.as_deref(),
                                    &request.path,
                                )
                            };
                            for cookie in take_response_cookies() {
                                response.headers.push(("Set-Cookie".into(), cookie));
                            }
                            let middlewares = [Middleware::SecurityHeaders, Middleware::RequestLog];
                            for m in &middlewares {
                                m.apply(&request, &mut response);
                            }
                            HttpServerState::write_response(&mut stream, &response);
                            break;
                        }
                        Ok(httparse::Status::Partial) => continue,
                        Err(_) => {
                            HttpServerState::write_response(
                                &mut stream,
                                &HttpResponse {
                                    status: 400,
                                    headers: vec![("Content-Type".into(), "text/plain".into())],
                                    body: b"Bad request".to_vec(),
                                },
                            );
                            break;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn serve_sse(room: &str, stream: &mut TcpStream, shutdown: Arc<AtomicBool>) {
        let (history, rx) = realtime_subscribe(room);
        let mut out = Vec::new();
        out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        out.extend_from_slice(b"Content-Type: text/event-stream\r\n");
        out.extend_from_slice(b"Cache-Control: no-cache\r\n");
        out.extend_from_slice(b"Connection: keep-alive\r\n");
        out.extend_from_slice(b"\r\n");
        let _ = stream.write_all(&out);

        for msg in history {
            if Self::write_sse_event(stream, &msg).is_err() {
                return;
            }
        }

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(msg) => {
                    if Self::write_sse_event(stream, &msg).is_err() {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn write_sse_event(stream: &mut TcpStream, msg: &str) -> std::io::Result<()> {
        let json = serde_json::to_string(msg).unwrap_or_else(|_| "null".to_string());
        stream.write_all(b"data: ")?;
        stream.write_all(json.as_bytes())?;
        stream.write_all(b"\n\n")?;
        stream.flush()
    }

    fn serve_static(
        static_dir: Option<&Path>,
        output_dir: Option<&Path>,
        path: &str,
    ) -> HttpResponse {
        let trimmed = path.trim_start_matches('/');
        let file_path = static_dir
            .map(|d| {
                let p = d.join(trimmed);
                if trimmed.is_empty() || p.is_dir() {
                    d.join("index.html")
                } else {
                    p
                }
            })
            .filter(|p| p.is_file())
            .or_else(|| {
                output_dir
                    .map(|d| {
                        let p = d.join(trimmed);
                        if trimmed.is_empty() || p.is_dir() {
                            d.join("index.html")
                        } else {
                            p
                        }
                    })
                    .filter(|p| p.is_file())
            });

        if let Some(p) = file_path {
            if let Ok(data) = std::fs::read(&p) {
                let content_type = guess_content_type(&p);
                return HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), content_type.into())],
                    body: data,
                };
            }
        }
        HttpResponse {
            status: 404,
            headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
            body: b"Not found".to_vec(),
        }
    }
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_route_exact() {
        let params = match_route("/", "/").unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn test_match_route_no_match() {
        assert!(match_route("/foo", "/bar").is_none());
    }

    #[test]
    fn test_match_route_captures_param() {
        let params = match_route("/products/:id", "/products/42").unwrap();
        assert_eq!(params.get("id"), Some(&"42".to_string()));
    }

    #[test]
    fn test_match_route_length_mismatch() {
        assert!(match_route("/products/:id", "/products/42/extra").is_none());
    }

    #[test]
    fn test_parse_form_urlencoded() {
        let pairs = parse_form_urlencoded(b"title=Buy+milk&id=1&foo=%26%3D");
        assert_eq!(pairs.len(), 3);
        assert!(pairs.contains(&("title".to_string(), "Buy milk".to_string())));
        assert!(pairs.contains(&("id".to_string(), "1".to_string())));
        assert!(pairs.contains(&("foo".to_string(), "&=".to_string())));
    }

    #[test]
    fn test_kv_store_roundtrip() {
        kv_set("todos", "buy milk");
        assert_eq!(kv_get("todos"), Some("buy milk".to_string()));
        kv_delete("todos");
        assert_eq!(kv_get("todos"), None);
    }

    #[test]
    fn test_form_value_lookup() {
        let _ctx = with_request_context(
            RequestContext {
                request: HttpRequest {
                    method: HttpMethod::Post,
                    path: "/".to_string(),
                    headers: vec![],
                    body: b"__nulang_action=add_todo&title=Buy+milk".to_vec(),
                },
                params: HashMap::new(),
            },
            || {
                assert_eq!(form_value("__nulang_action"), Some("add_todo".to_string()));
                assert_eq!(form_value("title"), Some("Buy milk".to_string()));
                assert_eq!(form_value("missing"), None);
            },
        );
    }

    #[test]
    fn test_current_request_method() {
        let _ctx = with_request_context(
            RequestContext {
                request: HttpRequest {
                    method: HttpMethod::Post,
                    path: "/".to_string(),
                    headers: vec![],
                    body: vec![],
                },
                params: HashMap::new(),
            },
            || {
                assert_eq!(current_request_method(), Some("POST".to_string()));
            },
        );
    }

    #[test]
    fn test_middleware_security_headers() {
        let mut response = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: vec![],
        };
        let request = HttpRequest {
            method: HttpMethod::Get,
            path: "/".into(),
            headers: vec![],
            body: vec![],
        };
        Middleware::SecurityHeaders.apply(&request, &mut response);
        let names: Vec<&str> = response.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"X-Content-Type-Options"));
        assert!(names.contains(&"X-Frame-Options"));
    }

    #[test]
    fn test_realtime_broadcast_history() {
        let room = "test-room";
        realtime_broadcast(room, "hello");
        let (history, _rx) = realtime_subscribe(room);
        assert_eq!(history, vec!["hello"]);
    }
}
