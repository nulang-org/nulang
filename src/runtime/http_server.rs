//! HTTP/1.1 server built on std::net::TcpListener with httparse.
//!
//! Phase 1: each request dispatches to a standalone VM on the listener thread.
//! Handlers are non-capturing `fn(String) -> String` — no closures with env.
//! Status is always 200; Content-Type is always text/plain.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::bytecode::CodeModule;
use crate::value_layout::PAYLOAD_MASK;
use crate::vm::{resolve_value_string, Value, CLOSURE_ENV_FLAG, VM};
use crate::web::csrf::validate_mutation_origin;
use crate::web::dispatch::{render_direct_request, DirectRequestRenderError};
use crate::web::http_problem::request_decode_problem_response;
use crate::web::http_request::HttpRequestBindingInputs;
use crate::web::reactivity::inject_client_runtime_script;
use crate::web::request_bindings::percent_decode_form;
use crate::web::runtime_bindings::RuntimeWebRoute;

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
                            let Some(method) = HttpMethod::from_str(req.method.unwrap_or(""))
                            else {
                                HttpServerState::write_response(
                                    &mut stream,
                                    &HttpResponse {
                                        status: 405,
                                        headers: vec![(
                                            "Content-Type".into(),
                                            "text/plain; charset=utf-8".into(),
                                        )],
                                        body: b"Method not allowed".to_vec(),
                                    },
                                );
                                break;
                            };
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
            303 => "See Other",
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
    static RESPONSE_REDIRECT: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

/// Set the request context for the current thread, run `f`, then clear it.
pub fn with_request_context<F, R>(ctx: RequestContext, f: F) -> R
where
    F: FnOnce() -> R,
{
    CURRENT_REQUEST.with(|c| *c.borrow_mut() = Some(ctx));
    RESPONSE_COOKIES.with(|c| c.borrow_mut().clear());
    RESPONSE_REDIRECT.with(|c| *c.borrow_mut() = None);
    let result = f();
    CURRENT_REQUEST.with(|c| *c.borrow_mut() = None);
    RESPONSE_REDIRECT.with(|c| *c.borrow_mut() = None);
    result
}

/// Mark a local-path redirect only while evaluating a routed HTTP request.
/// The returned Html must match the value generated by Web.redirect exactly;
/// discarded/interpolated redirect fragments never change the HTTP status.
pub fn note_web_redirect(url: &str, html: &str) {
    if !crate::web::redirect::is_safe_redirect_target(url) {
        return;
    }
    let active = CURRENT_REQUEST.with(|ctx| ctx.borrow().is_some());
    if active {
        RESPONSE_REDIRECT.with(|cell| {
            *cell.borrow_mut() = Some((url.to_string(), html.to_string()));
        });
    }
}

/// Consume the pending redirect only if it was returned as the complete
/// handler result. This must be called inside with_request_context.
fn take_web_redirect(rendered: &str) -> Option<String> {
    RESPONSE_REDIRECT
        .with(|cell| cell.borrow_mut().take())
        .and_then(|(url, expected_html)| (rendered == expected_html).then_some(url))
}

fn web_redirect_http_response(location: String) -> HttpResponse {
    HttpResponse {
        status: 303,
        headers: vec![
            ("Location".to_string(), location),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: Vec::new(),
    }
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
        let key = kv.next().unwrap_or("");
        let value = kv.next().unwrap_or("");
        pairs.push((percent_decode_form(key), percent_decode_form(value)));
    }
    pairs
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

// The experimental in-process SSE transport is intentionally bounded. These
// limits are per server process, NOT a durable/distributed delivery guarantee.
const MAX_REALTIME_ROOMS: usize = 128;
const MAX_REALTIME_ROOM_NAME_BYTES: usize = 128;
const MAX_REALTIME_HISTORY_MESSAGES: usize = 64;
const MAX_REALTIME_HISTORY_BYTES: usize = 64 * 1024;
const MAX_REALTIME_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_REALTIME_SUBSCRIBERS_PER_ROOM: usize = 8;
const MAX_REALTIME_SUBSCRIBER_BACKLOG: usize = 8;

static REALTIME_BROADCAST: OnceLock<Mutex<RealtimeRegistry>> = OnceLock::new();

fn realtime_broadcast_registry() -> &'static Mutex<RealtimeRegistry> {
    REALTIME_BROADCAST.get_or_init(|| Mutex::new(RealtimeRegistry::default()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RealtimeLimitError {
    InvalidRoom,
    MessageTooLarge,
    RoomCapacity,
    SubscriberCapacity,
}

#[derive(Default)]
struct RealtimeRegistry {
    rooms: HashMap<String, BroadcastRoom>,
    clock: u64,
    next_subscriber_id: u64,
}

struct BroadcastRoom {
    messages: VecDeque<String>,
    replay_bytes: usize,
    listeners: Vec<(u64, SyncSender<String>)>,
    last_used: u64,
}

impl RealtimeRegistry {
    fn validate_room(room: &str) -> Result<(), RealtimeLimitError> {
        if room.is_empty()
            || room.len() > MAX_REALTIME_ROOM_NAME_BYTES
            || !room
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
            || room
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(RealtimeLimitError::InvalidRoom);
        }
        Ok(())
    }

    fn room_mut(&mut self, room: &str) -> Result<&mut BroadcastRoom, RealtimeLimitError> {
        Self::validate_room(room)?;
        if !self.rooms.contains_key(room) {
            if self.rooms.len() >= MAX_REALTIME_ROOMS {
                // Evict only inactive rooms. Active SSE connections retain their
                // rooms and fail closed when every slot is occupied.
                let oldest = self
                    .rooms
                    .iter()
                    .filter(|(_, existing)| existing.listeners.is_empty())
                    .min_by_key(|(_, existing)| existing.last_used)
                    .map(|(name, _)| name.clone());
                if let Some(oldest) = oldest {
                    self.rooms.remove(&oldest);
                } else {
                    return Err(RealtimeLimitError::RoomCapacity);
                }
            }
            self.rooms.insert(
                room.to_owned(),
                BroadcastRoom {
                    messages: VecDeque::new(),
                    replay_bytes: 0,
                    listeners: Vec::new(),
                    last_used: 0,
                },
            );
        }
        self.clock = self.clock.wrapping_add(1);
        let entry = self.rooms.get_mut(room).expect("room was inserted above");
        entry.last_used = self.clock;
        Ok(entry)
    }

    fn broadcast(&mut self, room: &str, message: &str) -> Result<(), RealtimeLimitError> {
        if message.len() > MAX_REALTIME_MESSAGE_BYTES {
            return Err(RealtimeLimitError::MessageTooLarge);
        }
        let entry = self.room_mut(room)?;
        entry.messages.push_back(message.to_owned());
        entry.replay_bytes += message.len();
        while entry.messages.len() > MAX_REALTIME_HISTORY_MESSAGES
            || entry.replay_bytes > MAX_REALTIME_HISTORY_BYTES
        {
            if let Some(oldest) = entry.messages.pop_front() {
                entry.replay_bytes -= oldest.len();
            }
        }

        // Never block a producer on a disconnected or slow browser. Dropping
        // the sender eventually terminates the corresponding SSE stream.
        entry
            .listeners
            .retain(|(_, sender)| sender.try_send(message.to_owned()).is_ok());
        Ok(())
    }

    fn subscribe(
        &mut self,
        room: &str,
    ) -> Result<(u64, Vec<String>, Receiver<String>), RealtimeLimitError> {
        let entry = self.room_mut(room)?;
        if entry.listeners.len() >= MAX_REALTIME_SUBSCRIBERS_PER_ROOM {
            return Err(RealtimeLimitError::SubscriberCapacity);
        }
        let (tx, rx) = sync_channel(MAX_REALTIME_SUBSCRIBER_BACKLOG);
        let history: Vec<String> = entry.messages.iter().cloned().collect();
        self.next_subscriber_id = self.next_subscriber_id.wrapping_add(1);
        let id = self.next_subscriber_id;
        self.rooms
            .get_mut(room)
            .expect("room exists")
            .listeners
            .push((id, tx));
        Ok((id, history, rx))
    }

    fn unsubscribe(&mut self, room: &str, id: u64) {
        if let Some(entry) = self.rooms.get_mut(room) {
            entry
                .listeners
                .retain(|(subscriber_id, _)| *subscriber_id != id);
        }
    }
}

/// Lifetime guard that removes a subscriber immediately when the SSE stream
/// exits, even when no further messages arrive for the room.
struct RealtimeSubscription {
    room: String,
    id: u64,
    history: Vec<String>,
    rx: Receiver<String>,
}

impl Drop for RealtimeSubscription {
    fn drop(&mut self) {
        if let Ok(mut registry) = realtime_broadcast_registry().lock() {
            registry.unsubscribe(&self.room, self.id);
        }
    }
}

/// Broadcast to a bounded local room. Oversize messages or exhausted room
/// capacity are logged, never buffered indefinitely. This remains demo-only:
/// applications needing delivery/authorization guarantees must supply them.
pub fn realtime_broadcast(room: &str, message: &str) {
    if let Err(error) = realtime_broadcast_registry()
        .lock()
        .expect("realtime registry poisoned")
        .broadcast(room, message)
    {
        eprintln!("nulang realtime broadcast rejected for room {room:?}: {error:?}");
    }
}

fn realtime_subscribe(room: &str) -> Result<RealtimeSubscription, RealtimeLimitError> {
    let (id, history, rx) = realtime_broadcast_registry()
        .lock()
        .expect("realtime registry poisoned")
        .subscribe(room)?;
    Ok(RealtimeSubscription {
        room: room.to_owned(),
        id,
        history,
        rx,
    })
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
#[allow(dead_code)] // exercised by unit tests; production dispatch routes via `web::dispatch::match_route`
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

/// Reject unsafe browser requests before invoking any route. A trusted
/// reverse proxy must configure NULANG_WEB_PUBLIC_ORIGIN to its external origin.
/// This is CSRF provenance validation, not authentication or authorization.
fn unsafe_origin_response(
    request: &HttpRequest,
    local_addr: Option<std::net::SocketAddr>,
    public_origin: Option<&str>,
) -> Option<HttpResponse> {
    let failure = validate_mutation_origin(
        request.method.as_str(),
        &request.headers,
        local_addr,
        public_origin,
    )
    .err()?;
    eprintln!("Nulang Web denied state-changing request: {failure:?}");
    Some(HttpResponse {
        status: 403,
        headers: vec![
            ("Content-Type".into(), "text/plain; charset=utf-8".into()),
            ("Cache-Control".into(), "no-store".into()),
        ],
        body: b"Forbidden: invalid request origin".to_vec(),
    })
}

fn invalid_ui_action_response(captured: &HttpRequestBindingInputs) -> Option<HttpResponse> {
    captured
        .ui_message_error
        .as_ref()
        .map(|error| HttpResponse {
            status: 400,
            headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
            body: format!("Invalid UI action envelope: {error}").into_bytes(),
        })
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
        let routes = routes
            .into_iter()
            .map(|route| RuntimeWebRoute { route, plan: None })
            .collect();
        Self::bind_runtime(port, static_dir, output_dir, routes)
    }

    /// Bind a dev server using compiler-validated, precompiled route plans.
    ///
    /// `nula dev` uses this entry point after Web Contract IR validation.
    /// Legacy callers can continue using [`Self::bind`], which wraps raw
    /// registrations without opting them into direct typed invocation.
    pub fn bind_runtime(
        port: u16,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<RuntimeWebRoute>,
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
        routes: Vec<RuntimeWebRoute>,
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
        routes: Vec<RuntimeWebRoute>,
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
                            let Some(method) = HttpMethod::from_str(req.method.unwrap_or(""))
                            else {
                                HttpServerState::write_response(
                                    &mut stream,
                                    &HttpResponse {
                                        status: 405,
                                        headers: vec![(
                                            "Content-Type".into(),
                                            "text/plain; charset=utf-8".into(),
                                        )],
                                        body: b"Method not allowed".to_vec(),
                                    },
                                );
                                break;
                            };
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

                            // Verify browser provenance before route dispatch,
                            // including legacy ambient-context handlers.
                            let public_origin = std::env::var("NULANG_WEB_PUBLIC_ORIGIN").ok();
                            let mut response = if let Some(forbidden) = unsafe_origin_response(
                                &request,
                                stream.local_addr().ok(),
                                public_origin.as_deref(),
                            ) {
                                forbidden
                            } else if path.starts_with(Self::SSE_PATH_PREFIX) {
                                let room = &path[Self::SSE_PATH_PREFIX.len()..];
                                Self::serve_sse(room, &mut stream, shutdown.clone());
                                break;
                            } else if let Some((route, params)) = routes
                                .iter()
                                .filter(|r| r.route.method == request.method)
                                .find_map(|r| {
                                    crate::web::dispatch::match_route(r, &request.path)
                                        .map(|params| (r, params))
                                })
                            {
                                let captured = HttpRequestBindingInputs::capture(
                                    &request.path,
                                    &request.headers,
                                    &request.body,
                                );
                                if let Some(response) = invalid_ui_action_response(&captured) {
                                    response
                                } else {
                                    let values = captured.values(&params, &request.headers);
                                    let ctx = RequestContext {
                                        request: request.clone(),
                                        params: params.clone(),
                                    };
                                    let (rendered, redirect) = with_request_context(ctx, || {
                                        let rendered = match render_direct_request(route, &values) {
                                            Ok(Some(rendered)) => Ok(Some(rendered)),
                                            Ok(None) => Ok(render_route_handler(
                                                &route.route.handler_module,
                                                route.route.handler_func_idx,
                                                None,
                                            )),
                                            Err(error) => Err(error),
                                        };
                                        let redirect = rendered
                                            .as_ref()
                                            .ok()
                                            .and_then(|value| value.as_deref())
                                            .and_then(take_web_redirect);
                                        (rendered, redirect)
                                    });
                                    match rendered {
                                        Ok(Some(html)) => {
                                            if let Some(location) = redirect {
                                                web_redirect_http_response(location)
                                            } else {
                                                HttpResponse {
                                                    status: 200,
                                                    headers: vec![(
                                                        "Content-Type".into(),
                                                        "text/html; charset=utf-8".into(),
                                                    )],
                                                    body: inject_client_runtime_script(&html)
                                                        .into_bytes(),
                                                }
                                            }
                                        }
                                        Ok(None) => HttpResponse {
                                            status: 500,
                                            headers: vec![(
                                                "Content-Type".into(),
                                                "text/plain".into(),
                                            )],
                                            body: b"Internal server error".to_vec(),
                                        },
                                        Err(DirectRequestRenderError::Decode(error)) => {
                                            let problem = request_decode_problem_response(&error);
                                            HttpResponse {
                                                status: problem.status,
                                                headers: problem.headers,
                                                body: problem.body,
                                            }
                                        }
                                        Err(DirectRequestRenderError::Execution(error)) => {
                                            eprintln!("typed route dispatch error: {error}");
                                            HttpResponse {
                                                status: 500,
                                                headers: vec![(
                                                    "Content-Type".into(),
                                                    "text/plain".into(),
                                                )],
                                                body: b"Internal server error".to_vec(),
                                            }
                                        }
                                    }
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
        let subscription = match realtime_subscribe(room) {
            Ok(subscription) => subscription,
            Err(error) => {
                let status = if error == RealtimeLimitError::InvalidRoom {
                    400
                } else {
                    503
                };
                HttpServerState::write_response(
                    stream,
                    &HttpResponse {
                        status,
                        headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
                        body: format!("SSE subscription rejected: {error:?}").into_bytes(),
                    },
                );
                return;
            }
        };
        // A stalled TCP reader must not hold an SSE thread indefinitely.
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let mut out = Vec::new();
        out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        out.extend_from_slice(b"Content-Type: text/event-stream\r\n");
        out.extend_from_slice(b"Cache-Control: no-cache\r\n");
        out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
        out.extend_from_slice(b"Connection: keep-alive\r\n");
        out.extend_from_slice(b"\r\n");
        let _ = stream.write_all(&out);

        for msg in &subscription.history {
            if Self::write_sse_event(stream, msg).is_err() {
                return;
            }
        }

        let mut last_write = Instant::now();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match subscription.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(msg) => {
                    if Self::write_sse_event(stream, &msg).is_err() {
                        break;
                    }
                    last_write = Instant::now();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Test the TCP connection even when the room is idle.
                    // Otherwise abandoned EventSource subscriptions can hold
                    // room slots indefinitely without further broadcasts.
                    if last_write.elapsed() >= Duration::from_secs(15) {
                        if Self::write_sse_keepalive(stream).is_err() {
                            break;
                        }
                        last_write = Instant::now();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn write_sse_keepalive<W: Write>(stream: &mut W) -> std::io::Result<()> {
        stream.write_all(b": keep-alive\n\n")?;
        stream.flush()
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
        // The raw request target may include a query string; it is not a
        // filesystem path. Reject parent/current/prefix components outright.
        let relative = path.split('?').next().unwrap_or("").trim_start_matches('/');
        let candidate = static_dir
            .and_then(|root| safe_static_candidate(root, relative))
            .or_else(|| output_dir.and_then(|root| safe_static_candidate(root, relative)));

        if let Some(file) = candidate {
            if let Ok(data) = std::fs::read(&file) {
                let content_type = guess_content_type(&file);
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

/// Resolve existing files against a canonical static root. Reject traversal
/// and symlinks resolving outside that root; configured static directories
/// must themselves be trusted and not writable by untrusted callers.
fn safe_static_candidate(root: &Path, relative: &str) -> Option<PathBuf> {
    let rel = Path::new(relative);
    if rel
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = root.join(rel);
    let candidate = if relative.is_empty() || candidate.is_dir() {
        candidate.join("index.html")
    } else {
        candidate
    };
    let resolved = candidate.canonicalize().ok()?;
    if resolved.starts_with(&root) && resolved.is_file() {
        Some(resolved)
    } else {
        None
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
        let pairs = parse_form_urlencoded(b"title=Buy+milk&id=1&foo=%26%3D&name=%C3%A9");
        assert_eq!(pairs.len(), 4);
        assert!(pairs.contains(&("title".to_string(), "Buy milk".to_string())));
        assert!(pairs.contains(&("id".to_string(), "1".to_string())));
        assert!(pairs.contains(&("foo".to_string(), "&=".to_string())));
        assert!(pairs.contains(&("name".to_string(), "é".to_string())));
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
    fn invalid_ui_action_envelope_is_rejected_before_route_dispatch() {
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        let captured = HttpRequestBindingInputs::capture(
            "/",
            &headers,
            b"__nulang_ui_message=%7B%22type%22%3A%22invoke_action%22%7D",
        );

        let response = invalid_ui_action_response(&captured)
            .expect("malformed envelope must fail closed before route dispatch");
        assert_eq!(response.status, 400);
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("Invalid UI action envelope"));
    }

    #[test]
    fn web_redirect_is_scoped_to_the_request_and_final_return_value() {
        let request = RequestContext {
            request: HttpRequest {
                method: HttpMethod::Post,
                path: "/sign".to_string(),
                headers: vec![],
                body: vec![],
            },
            params: HashMap::new(),
        };
        let legacy =
            r#"<html><head><meta http-equiv="refresh" content="0;url=/receipt"></head></html>"#
                .to_string();

        assert_eq!(take_web_redirect(&legacy), None);
        with_request_context(request.clone(), || {
            note_web_redirect("/receipt", &legacy);
            assert_eq!(take_web_redirect("<p>Ignored redirect</p>"), None);
            note_web_redirect("/receipt", &legacy);
            assert_eq!(take_web_redirect(&legacy), Some("/receipt".to_string()));
        });
        assert_eq!(take_web_redirect(&legacy), None);

        with_request_context(request, || {
            note_web_redirect("//evil.example", &legacy);
            assert_eq!(take_web_redirect(&legacy), None);
        });
    }

    #[test]
    fn web_redirect_http_response_has_real_303_and_no_body() {
        let response = web_redirect_http_response("/receipt?ok=yes".to_string());
        assert_eq!(response.status, 303);
        assert!(response.body.is_empty());
        assert!(response
            .headers
            .contains(&("Location".to_string(), "/receipt?ok=yes".to_string())));
        assert!(response
            .headers
            .contains(&("Cache-Control".to_string(), "no-store".to_string())));
    }

    #[test]
    fn web_dev_server_rejects_foreign_posts_and_unknown_http_methods_over_tcp() {
        fn send(port: u16, raw: &str) -> String {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        }

        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let host = format!("localhost:{}", server.port);
        let foreign = format!(
            "POST /sign HTTP/1.1\r\nHost: {host}\r\nOrigin: https://attacker.example\r\nContent-Length: 0\r\n\r\n"
        );
        assert!(send(server.port, &foreign).starts_with("HTTP/1.1 403 Forbidden"));

        let missing = format!("POST /sign HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\n\r\n");
        assert!(send(server.port, &missing).starts_with("HTTP/1.1 403 Forbidden"));

        let trusted = format!(
            "POST /sign HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Length: 0\r\n\r\n"
        );
        assert!(
            send(server.port, &trusted).starts_with("HTTP/1.1 404 Not Found"),
            "trusted provenance must reach routing instead of being rejected"
        );

        let unknown = format!("TRACE / HTTP/1.1\r\nHost: {host}\r\n\r\n");
        assert!(send(server.port, &unknown).starts_with("HTTP/1.1 405 Method Not Allowed"));
    }

    #[test]
    fn mutation_provenance_is_checked_before_route_execution() {
        let mut request = HttpRequest {
            method: HttpMethod::Post,
            path: "/sign".to_string(),
            headers: vec![("Host".into(), "localhost:8080".into())],
            body: b"msg=untrusted".to_vec(),
        };
        let local = Some("127.0.0.1:8080".parse().unwrap());
        let rejected = unsafe_origin_response(&request, local, None)
            .expect("missing browser provenance must stop route execution");
        assert_eq!(rejected.status, 403);
        assert!(String::from_utf8_lossy(&rejected.body).contains("invalid request origin"));

        request
            .headers
            .push(("Origin".into(), "http://localhost:8080".into()));
        assert!(unsafe_origin_response(&request, local, None).is_none());
        request.method = HttpMethod::Get;
        request.headers.clear();
        assert!(unsafe_origin_response(&request, local, None).is_none());
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
        let subscription = realtime_subscribe(room).unwrap();
        assert_eq!(subscription.history, vec!["hello"]);
    }

    #[test]
    fn static_serving_stays_inside_its_root_including_symlinks() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("nulang-web-root-{}-{nonce}", std::process::id()));
        let public = dir.join("public");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("index.html"), "public").unwrap();
        std::fs::write(dir.join("private.txt"), "sensitive").unwrap();

        assert_eq!(
            WebDevServer::serve_static(Some(&public), None, "/index.html").status,
            200
        );
        assert_eq!(
            WebDevServer::serve_static(Some(&public), None, "/../private.txt").status,
            404
        );
        assert_eq!(
            WebDevServer::serve_static(Some(&public), None, "/inside/../../private.txt").status,
            404
        );
        assert_eq!(
            WebDevServer::serve_static(Some(&public), None, "/index.html?v=1").status,
            200
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("private.txt"), public.join("alias.txt")).unwrap();
            assert_eq!(
                WebDevServer::serve_static(Some(&public), None, "/alias.txt").status,
                404
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn realtime_room_replay_and_slow_subscribers_are_bounded() {
        let mut registry = RealtimeRegistry::default();
        let (_id, _history, slow) = registry.subscribe("unit:bounded").unwrap();
        for n in 0..(MAX_REALTIME_HISTORY_MESSAGES + MAX_REALTIME_SUBSCRIBER_BACKLOG + 4) {
            registry
                .broadcast("unit:bounded", &format!("event-{n}"))
                .unwrap();
        }
        let room = registry.rooms.get("unit:bounded").unwrap();
        assert_eq!(room.messages.len(), MAX_REALTIME_HISTORY_MESSAGES);
        assert!(room.replay_bytes <= MAX_REALTIME_HISTORY_BYTES);
        assert!(
            room.listeners.is_empty(),
            "slow client must be disconnected, not buffered forever"
        );
        assert!(slow.try_iter().count() <= MAX_REALTIME_SUBSCRIBER_BACKLOG);

        let (_id, replay, _live) = registry.subscribe("unit:bounded").unwrap();
        assert_eq!(replay.len(), MAX_REALTIME_HISTORY_MESSAGES);
        let latest = format!(
            "event-{}",
            MAX_REALTIME_HISTORY_MESSAGES + MAX_REALTIME_SUBSCRIBER_BACKLOG + 3
        );
        assert_eq!(replay.last().map(String::as_str), Some(latest.as_str()));
        assert!(registry
            .broadcast("unit:bounded", &"x".repeat(MAX_REALTIME_MESSAGE_BYTES + 1))
            .is_err());
    }

    #[test]
    fn realtime_history_is_bounded_by_bytes_as_well_as_event_count() {
        let mut registry = RealtimeRegistry::default();
        for n in 0..32 {
            registry
                .broadcast(
                    "unit:bytes",
                    &format!("{n:02}{}", "x".repeat(MAX_REALTIME_MESSAGE_BYTES - 2)),
                )
                .unwrap();
        }
        let room = registry.rooms.get("unit:bytes").unwrap();
        assert!(room.replay_bytes <= MAX_REALTIME_HISTORY_BYTES);
        assert!(room.messages.len() <= MAX_REALTIME_HISTORY_BYTES / MAX_REALTIME_MESSAGE_BYTES);
    }

    #[test]
    fn sse_heartbeat_is_a_comment_and_not_an_application_message() {
        let mut wire = Vec::new();
        WebDevServer::write_sse_keepalive(&mut wire).unwrap();
        assert_eq!(wire.as_slice(), b": keep-alive\n\n");
    }

    #[test]
    fn realtime_registry_preserves_active_rooms_at_capacity() {
        let mut registry = RealtimeRegistry::default();
        let mut active = Vec::new();
        for n in 0..MAX_REALTIME_ROOMS {
            let (_, _, receiver) = registry.subscribe(&format!("active:{n}")).unwrap();
            active.push(receiver);
        }
        assert_eq!(registry.rooms.len(), MAX_REALTIME_ROOMS);
        assert_eq!(
            registry.broadcast("active:new", "message"),
            Err(RealtimeLimitError::RoomCapacity)
        );
        assert_eq!(registry.rooms.len(), MAX_REALTIME_ROOMS);
    }

    #[test]
    fn realtime_registry_bounds_rooms_and_subscribers() {
        let mut registry = RealtimeRegistry::default();
        for n in 0..MAX_REALTIME_ROOMS {
            registry.broadcast(&format!("unit:{n}"), "initial").unwrap();
        }
        registry.broadcast("unit:new", "recent").unwrap();
        assert_eq!(registry.rooms.len(), MAX_REALTIME_ROOMS);
        assert!(
            !registry.rooms.contains_key("unit:0"),
            "oldest room without subscribers is evicted"
        );

        let mut subscriptions = Vec::new();
        for _ in 0..MAX_REALTIME_SUBSCRIBERS_PER_ROOM {
            let (_, _, receiver) = registry.subscribe("unit:new").unwrap();
            subscriptions.push(receiver);
        }
        assert!(registry.subscribe("unit:new").is_err());
        assert!(registry.subscribe("../unsafe").is_err());
    }
}
