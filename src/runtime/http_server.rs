//! HTTP/1.1 server built on std::net::TcpListener with httparse.
//!
//! Phase 1: each request dispatches to a standalone VM on the listener thread.
//! Handlers are non-capturing `fn(String) -> String` — no closures with env.
//! Status is always 200; Content-Type is always text/plain.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::bytecode::CodeModule;
use crate::value_layout::PAYLOAD_MASK;
use crate::vm::{resolve_value_string, Value, CLOSURE_ENV_FLAG, VM};
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

/// Framing errors are rejected before executing a user handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpReadError {
    BadRequest,
    Timeout,
    PayloadTooLarge,
    HeadersTooLarge,
    ExpectationFailed,
    NotImplemented,
}

impl HttpReadError {
    fn response(self) -> HttpResponse {
        let (status, message) = match self {
            Self::BadRequest => (400, "Bad request"),
            Self::Timeout => (408, "Request timeout"),
            Self::PayloadTooLarge => (413, "Payload too large"),
            Self::HeadersTooLarge => (431, "Request headers too large"),
            Self::ExpectationFailed => (417, "Expectation failed"),
            Self::NotImplemented => (501, "Unsupported HTTP method or transfer encoding"),
        };
        HttpResponse {
            status,
            headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
            body: message.as_bytes().to_vec(),
        }
    }
}

const MAX_HTTP_HEADERS_SIZE: usize = 16 * 1024;
const MAX_HTTP_BODY_SIZE: usize = 1_048_576;

/// Reads exactly one HTTP/1.x request. Both HTTP listeners use this same
/// bounded framing boundary, rather than assuming one read contains a request.
///
/// Chunked bodies and Expect: 100-continue are rejected explicitly until
/// supported; silently dispatching a truncated or ambiguously framed body is
/// unsafe. Neither server currently supports persistent HTTP connections.
fn read_http_request<R: Read>(reader: &mut R) -> Result<Option<HttpRequest>, HttpReadError> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        let mut header_slots = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut header_slots);
        match request.parse(&bytes) {
            Ok(httparse::Status::Complete(end)) => {
                if end > MAX_HTTP_HEADERS_SIZE {
                    return Err(HttpReadError::HeadersTooLarge);
                }
                break end;
            }
            Ok(httparse::Status::Partial) => {
                if bytes.len() >= MAX_HTTP_HEADERS_SIZE {
                    return Err(HttpReadError::HeadersTooLarge);
                }
            }
            Err(httparse::Error::TooManyHeaders) => {
                return Err(HttpReadError::HeadersTooLarge);
            }
            Err(_) => return Err(HttpReadError::BadRequest),
        }

        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk) {
            Ok(0) if bytes.is_empty() => return Ok(None),
            Ok(0) => return Err(HttpReadError::BadRequest),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(HttpReadError::Timeout);
            }
            Err(_) => return Err(HttpReadError::BadRequest),
        }
    };

    let mut header_slots = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut header_slots);
    if !matches!(
        parsed.parse(&bytes[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(HttpReadError::BadRequest);
    }

    let method = parsed
        .method
        .and_then(HttpMethod::from_str)
        .ok_or(HttpReadError::NotImplemented)?;
    let path = parsed.path.ok_or(HttpReadError::BadRequest)?.to_string();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_string(),
                String::from_utf8_lossy(header.value).to_string(),
            )
        })
        .collect();

    let mut content_length = None;
    let mut has_transfer_encoding = false;
    let mut has_expectation = false;
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("content-length") {
            // Reject even identical duplicates: this is a one-request server,
            // and accepting ambiguous lengths enables request desynchronization.
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpReadError::BadRequest);
            }
            let length = value
                .parse::<usize>()
                .map_err(|_| HttpReadError::BadRequest)?;
            if content_length.replace(length).is_some() {
                return Err(HttpReadError::BadRequest);
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            has_transfer_encoding = true;
        } else if name.eq_ignore_ascii_case("expect") {
            has_expectation = true;
        }
    }
    if has_transfer_encoding && content_length.is_some() {
        return Err(HttpReadError::BadRequest);
    }
    if has_expectation {
        return Err(HttpReadError::ExpectationFailed);
    }
    if has_transfer_encoding {
        return Err(HttpReadError::NotImplemented);
    }

    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_HTTP_BODY_SIZE {
        return Err(HttpReadError::PayloadTooLarge);
    }

    // Limit the body to Content-Length even if the first read also contained
    // the start of a pipelined request. We close after this one request.
    let buffered_body = &bytes[header_end..];
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&buffered_body[..buffered_body.len().min(content_length)]);
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let remaining = (content_length - body.len()).min(chunk.len());
        match reader.read(&mut chunk[..remaining]) {
            Ok(0) => return Err(HttpReadError::BadRequest),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(HttpReadError::Timeout);
            }
            Err(_) => return Err(HttpReadError::BadRequest),
        }
    }

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
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
        let request = match read_http_request(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) => {
                Self::write_response(&mut stream, &error.response());
                return;
            }
        };
        let response = Self::dispatch(handler_module, handler_func_idx, &request);
        Self::write_response(&mut stream, &response);
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
            408 => "Request Timeout",
            413 => "Payload Too Large",
            417 => "Expectation Failed",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Unknown",
        };
        let mut out = format!("HTTP/1.1 {} {}\r\n", response.status, status_text);
        for (k, v) in &response.headers {
            out.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !response
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("connection"))
        {
            out.push_str("Connection: close\r\n");
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
// Revisioned HTML-over-SSE stream (separate from legacy unbounded broadcast).
// Room state contains a bounded snapshot, not an unbounded event log.
// ---------------------------------------------------------------------------

const UI_STREAM_MAX_ROOMS: usize = 32;
const UI_STREAM_MAX_TARGETS: usize = 32;
const UI_STREAM_MAX_LISTENERS: usize = 8;
const UI_STREAM_MAX_HTML_BYTES: usize = 64 * 1024;
const UI_STREAM_CHANNEL_DEPTH: usize = 8;

#[derive(Debug)]
struct UiStreamRoom {
    revision: u64,
    elements: BTreeMap<String, String>,
    listeners: Vec<(u64, SyncSender<nulang_ui_protocol::UiHtmlStreamMessage>)>,
    /// Actor-owned private rooms require an alive actor binding to publish.
    owner_actor: Option<u64>,
    /// A room's unique ownership epoch prevents late writes from an old
    /// handle after cancellation followed by rebinding the same actor.
    owner_lease: Option<u64>,
}

struct UiStreamSubscriber {
    room: String,
    id: u64,
    rx: Receiver<nulang_ui_protocol::UiHtmlStreamMessage>,
}

impl UiStreamSubscriber {
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<nulang_ui_protocol::UiHtmlStreamMessage, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    #[cfg(test)]
    fn try_recv(
        &self,
    ) -> Result<nulang_ui_protocol::UiHtmlStreamMessage, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for UiStreamSubscriber {
    fn drop(&mut self) {
        if let Some(entry) = ui_stream_rooms().lock().unwrap().get_mut(&self.room) {
            entry.listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

static UI_STREAM_LISTENER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static UI_STREAM_ROOMS: OnceLock<Mutex<HashMap<String, UiStreamRoom>>> = OnceLock::new();

fn ui_stream_rooms() -> &'static Mutex<HashMap<String, UiStreamRoom>> {
    UI_STREAM_ROOMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publish one target update, returning false if room, selector, or bounded
/// storage cannot admit it. A full per-subscriber channel removes the slow
/// listener; it must reconnect to recover from the latest snapshot.
pub fn ui_stream_publish(room: &str, target: &str, html: &str) -> bool {
    use nulang_ui_protocol::{DocumentId, Revision, UiHtmlElement, UiHtmlStreamMessage};
    if room.is_empty()
        || room.len() > 128
        || !room
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        || html.len() > UI_STREAM_MAX_HTML_BYTES
    {
        return false;
    }
    let element = UiHtmlElement {
        target: target.into(),
        html: html.into(),
    };
    if UiHtmlStreamMessage::patch(
        DocumentId::from(room),
        Revision(0),
        Revision(1),
        element.clone(),
    )
    .validate()
    .is_err()
    {
        return false;
    }
    let mut rooms = ui_stream_rooms().lock().unwrap();
    if !rooms.contains_key(room) && rooms.len() >= UI_STREAM_MAX_ROOMS {
        return false;
    }
    let entry = rooms
        .entry(room.to_string())
        .or_insert_with(|| UiStreamRoom {
            revision: 0,
            elements: BTreeMap::new(),
            listeners: Vec::new(),
            owner_actor: None,
            owner_lease: None,
        });
    if !entry.elements.contains_key(target) && entry.elements.len() >= UI_STREAM_MAX_TARGETS {
        return false;
    }
    let Some(next) = entry.revision.checked_add(1) else {
        return false;
    };
    let patch = UiHtmlStreamMessage::patch(
        DocumentId::from(room),
        Revision(entry.revision),
        Revision(next),
        element,
    );
    entry.revision = next;
    entry.elements.insert(target.into(), html.into());
    entry
        .listeners
        .retain(|(_, sender)| match sender.try_send(patch.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        });
    true
}

/// Every subscription begins with an atomic current snapshot. This makes
/// Last-Event-ID / reconnect gap recovery independent of any event-history
/// retention and prevents reconnects from replaying stale mutations.
/// This first-generation stream has no authentication or tenant identity.
/// Restrict transport to same-origin browser access over loopback, regardless
/// of the dev server's broader listen address. Do not trust proxy headers.
fn ui_stream_local_request_allowed(request: &HttpRequest, peer: std::net::IpAddr) -> bool {
    if !peer.is_loopback() {
        return false;
    }

    // Accept only literal loopback authorities. Reject DNS rebinding via an
    // attacker-controlled Host even if it resolves to 127.0.0.1.
    let mut hosts = request
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("host"));
    let Some((_, host)) = hosts.next() else {
        return false;
    };
    if hosts.next().is_some() {
        return false;
    }
    let host = host.trim();
    let allowed_host = ["localhost", "127.0.0.1", "[::1]"].iter().any(|name| {
        if host.eq_ignore_ascii_case(name) {
            return true;
        }
        host.get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
            && host.get(name.len()..).is_some_and(|suffix| {
                suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty()
                        && port.bytes().all(|b| b.is_ascii_digit())
                        && port.parse::<u16>().is_ok_and(|port| port != 0)
                })
            })
    });
    if !allowed_host {
        return false;
    }

    // Browser EventSource is same-origin. Cross-site scripts should never
    // consume local SSE connections, including DNS-rebinding attempts.
    let mut origins = request
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("origin"));
    if let Some((_, origin)) = origins.next() {
        if !origin
            .trim()
            .eq_ignore_ascii_case(&format!("http://{host}"))
        {
            return false;
        }
    }
    if origins.next().is_some() {
        return false;
    }

    let mut fetch_sites = request
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("sec-fetch-site"));
    if let Some((_, site)) = fetch_sites.next() {
        if !matches!(site.trim(), "same-origin" | "none") {
            return false;
        }
    }
    fetch_sites.next().is_none()
}

fn ui_stream_subscribe(
    room: &str,
) -> Option<(nulang_ui_protocol::UiHtmlStreamMessage, UiStreamSubscriber)> {
    use nulang_ui_protocol::{DocumentId, Revision, UiHtmlElement, UiHtmlStreamMessage};
    if room.is_empty()
        || room.len() > 128
        || !room
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return None;
    }
    let mut rooms = ui_stream_rooms().lock().unwrap();
    if !rooms.contains_key(room) && rooms.len() >= UI_STREAM_MAX_ROOMS {
        return None;
    }
    let entry = rooms
        .entry(room.to_string())
        .or_insert_with(|| UiStreamRoom {
            revision: 0,
            elements: BTreeMap::new(),
            listeners: Vec::new(),
            owner_actor: None,
            owner_lease: None,
        });
    if entry.listeners.len() >= UI_STREAM_MAX_LISTENERS {
        return None;
    }
    let snapshot = UiHtmlStreamMessage::snapshot(
        DocumentId::from(room),
        Revision(entry.revision),
        entry
            .elements
            .iter()
            .map(|(target, html)| UiHtmlElement {
                target: target.clone(),
                html: html.clone(),
            })
            .collect(),
    );
    let (tx, rx) = sync_channel(UI_STREAM_CHANNEL_DEPTH);
    let id = UI_STREAM_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
    entry.listeners.push((id, tx));
    Some((
        snapshot,
        UiStreamSubscriber {
            room: room.to_string(),
            id,
            rx,
        },
    ))
}

// Authenticated application streams have a separate per-server registry.
// Only a trusted host may supply the authorizer and publishing handle.
pub type UiStreamAuthorizer = Arc<dyn Fn(&HttpRequest, &str, &str) -> bool + Send + Sync + 'static>;

#[derive(Debug, Default)]
struct ScopedUiStreams {
    rooms: Mutex<HashMap<(String, String), UiStreamRoom>>,
}

#[derive(Clone, Debug)]
pub struct ScopedUiPublisher {
    streams: Arc<ScopedUiStreams>,
}

impl ScopedUiPublisher {
    /// Bind an existing local Nulang actor to a private room, exclusively.
    /// Actor exit, cancellation, or dropping the last handle disconnects
    /// subscribers and clears the old HTML snapshot.
    pub fn bind_actor(
        &self,
        runtime: &mut super::Runtime,
        actor_id: u64,
        app: &str,
        room: &str,
    ) -> Option<ScopedUiActorBinding> {
        if !valid_scoped_ui_id(app)
            || !valid_scoped_ui_id(room)
            || actor_id == 0
            || !runtime
                .actors
                .get(&actor_id)
                .is_some_and(|actor| actor.state != super::ActorState::Terminated)
        {
            return None;
        }
        let alive = runtime
            .actor_ui_stream_bindings
            .entry(actor_id)
            .or_default();
        alive.retain(|weak| {
            weak.upgrade()
                .is_some_and(|binding| binding.active.load(Ordering::Acquire))
        });
        if alive.len() >= UI_STREAM_MAX_ROOMS {
            return None;
        }
        let key = (app.to_string(), room.to_string());
        let lease_id = UI_STREAM_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
        {
            let mut rooms = self.streams.rooms.lock().unwrap();
            if !rooms.contains_key(&key) && rooms.len() >= UI_STREAM_MAX_ROOMS {
                return None;
            }
            let entry = rooms.entry(key.clone()).or_insert_with(|| UiStreamRoom {
                revision: 0,
                elements: BTreeMap::new(),
                listeners: Vec::new(),
                owner_actor: None,
                owner_lease: None,
            });
            if entry.owner_actor.is_some()
                || entry.revision != 0
                || !entry.elements.is_empty()
                || !entry.listeners.is_empty()
            {
                return None;
            }
            entry.owner_actor = Some(actor_id);
            entry.owner_lease = Some(lease_id);
        }
        let inner = Arc::new(UiActorStreamBindingInner {
            actor_id,
            lease_id,
            runtime_token: Arc::downgrade(&runtime.actor_ui_stream_token),
            key,
            streams: Arc::clone(&self.streams),
            active: AtomicBool::new(true),
        });
        alive.push(Arc::downgrade(&inner));
        Some(ScopedUiActorBinding { inner })
    }

    /// The trusted host publisher deliberately cannot overwrite actor rooms.
    pub fn publish(&self, app: &str, room: &str, target: &str, html: &str) -> bool {
        self.publish_for_owner(None, app, room, target, html)
    }

    fn publish_for_owner(
        &self,
        owner: Option<(u64, u64)>,
        app: &str,
        room: &str,
        target: &str,
        html: &str,
    ) -> bool {
        use nulang_ui_protocol::{DocumentId, Revision, UiHtmlElement, UiHtmlStreamMessage};
        if !valid_scoped_ui_id(app)
            || !valid_scoped_ui_id(room)
            || html.len() > UI_STREAM_MAX_HTML_BYTES
        {
            return false;
        }
        let document_id = DocumentId::from(format!("{app}/{room}"));
        let element = UiHtmlElement {
            target: target.to_string(),
            html: html.to_string(),
        };
        if UiHtmlStreamMessage::patch(
            document_id.clone(),
            Revision(0),
            Revision(1),
            element.clone(),
        )
        .validate()
        .is_err()
        {
            return false;
        }
        let mut rooms = self.streams.rooms.lock().unwrap();
        let key = (app.to_string(), room.to_string());
        if !rooms.contains_key(&key) && rooms.len() >= UI_STREAM_MAX_ROOMS {
            return false;
        }
        let entry = rooms.entry(key).or_insert_with(|| UiStreamRoom {
            revision: 0,
            elements: BTreeMap::new(),
            listeners: Vec::new(),
            owner_actor: None,
            owner_lease: None,
        });
        if entry.owner_actor != owner.map(|(actor, _)| actor)
            || entry.owner_lease != owner.map(|(_, lease)| lease)
        {
            return false;
        }
        if !entry.elements.contains_key(target) && entry.elements.len() >= UI_STREAM_MAX_TARGETS {
            return false;
        }
        let Some(next) = entry.revision.checked_add(1) else {
            return false;
        };
        let message = UiHtmlStreamMessage::patch(
            document_id,
            Revision(entry.revision),
            Revision(next),
            element,
        );
        entry.revision = next;
        entry.elements.insert(target.to_string(), html.to_string());
        entry
            .listeners
            .retain(|(_, sender)| sender.try_send(message.clone()).is_ok());
        true
    }
}

fn valid_scoped_ui_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

/// A room capability bound to one local Nulang actor. Host code must
/// dispatch calls from its receiving actor behavior, not forged identities.
pub struct ScopedUiActorBinding {
    inner: Arc<UiActorStreamBindingInner>,
}

pub(crate) struct UiActorStreamBindingInner {
    actor_id: u64,
    lease_id: u64,
    runtime_token: std::sync::Weak<()>,
    key: (String, String),
    streams: Arc<ScopedUiStreams>,
    active: AtomicBool,
}

impl UiActorStreamBindingInner {
    pub(crate) fn cancel(&self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut rooms = self.streams.rooms.lock().unwrap();
        if rooms.get(&self.key).is_some_and(|room| {
            room.owner_actor == Some(self.actor_id) && room.owner_lease == Some(self.lease_id)
        }) {
            // Dropping senders disconnects clients and discards stale state.
            rooms.remove(&self.key);
        }
    }
}

impl Drop for UiActorStreamBindingInner {
    fn drop(&mut self) {
        self.cancel()
    }
}

impl ScopedUiActorBinding {
    /// Requires the exact owner and live runtime actor.
    pub fn publish(
        &self,
        runtime: &super::Runtime,
        sender_actor: u64,
        target: &str,
        html: &str,
    ) -> bool {
        if !self.inner.active.load(Ordering::Acquire)
            || sender_actor != self.inner.actor_id
            || runtime.current_actor != Some(sender_actor)
            || !self
                .inner
                .runtime_token
                .upgrade()
                .is_some_and(|token| Arc::ptr_eq(&token, &runtime.actor_ui_stream_token))
            || !runtime
                .actors
                .get(&sender_actor)
                .is_some_and(|actor| actor.state != super::ActorState::Terminated)
        {
            return false;
        }
        ScopedUiPublisher {
            streams: Arc::clone(&self.inner.streams),
        }
        .publish_for_owner(
            Some((sender_actor, self.inner.lease_id)),
            &self.inner.key.0,
            &self.inner.key.1,
            target,
            html,
        )
    }

    pub fn cancel(&self) {
        self.inner.cancel()
    }
}

struct ScopedUiSubscriber {
    key: (String, String),
    id: u64,
    owner_lease: Option<u64>,
    rx: Receiver<nulang_ui_protocol::UiHtmlStreamMessage>,
    streams: Arc<ScopedUiStreams>,
}

impl ScopedUiSubscriber {
    /// A cancelled actor binding cannot replay old queued patches into a
    /// replacement room, even when the actor number and room key are reused.
    fn still_current(&self) -> bool {
        self.streams
            .rooms
            .lock()
            .unwrap()
            .get(&self.key)
            .is_some_and(|room| {
                room.owner_lease == self.owner_lease
                    && room.listeners.iter().any(|(id, _)| *id == self.id)
            })
    }
}

impl Drop for ScopedUiSubscriber {
    fn drop(&mut self) {
        if let Some(room) = self.streams.rooms.lock().unwrap().get_mut(&self.key) {
            room.listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

fn subscribe_scoped_ui(
    streams: Arc<ScopedUiStreams>,
    app: &str,
    room: &str,
) -> Option<(nulang_ui_protocol::UiHtmlStreamMessage, ScopedUiSubscriber)> {
    use nulang_ui_protocol::{DocumentId, Revision, UiHtmlElement, UiHtmlStreamMessage};
    if !valid_scoped_ui_id(app) || !valid_scoped_ui_id(room) {
        return None;
    }
    let key = (app.to_owned(), room.to_owned());
    let mut rooms = streams.rooms.lock().unwrap();
    if !rooms.contains_key(&key) && rooms.len() >= UI_STREAM_MAX_ROOMS {
        return None;
    }
    let entry = rooms.entry(key.clone()).or_insert_with(|| UiStreamRoom {
        revision: 0,
        elements: BTreeMap::new(),
        listeners: Vec::new(),
        owner_actor: None,
        owner_lease: None,
    });
    if entry.listeners.len() >= UI_STREAM_MAX_LISTENERS {
        return None;
    }
    let snapshot = UiHtmlStreamMessage::snapshot(
        DocumentId::from(format!("{app}/{room}")),
        Revision(entry.revision),
        entry
            .elements
            .iter()
            .map(|(target, html)| UiHtmlElement {
                target: target.clone(),
                html: html.clone(),
            })
            .collect(),
    );
    let owner_lease = entry.owner_lease;
    let (tx, rx) = sync_channel(UI_STREAM_CHANNEL_DEPTH);
    let id = UI_STREAM_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
    entry.listeners.push((id, tx));
    Some((
        snapshot,
        ScopedUiSubscriber {
            key,
            id,
            owner_lease,
            rx,
            streams: Arc::clone(&streams),
        },
    ))
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

/// Limits concurrently active development-server connections, including
/// long-lived SSE streams. Lease release is automatic on worker exit or panic.
/// This is capacity control, not an isolation boundary for untrusted code.
struct ConnectionSlots {
    active: AtomicUsize,
    limit: usize,
}

impl ConnectionSlots {
    fn new(limit: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionLease> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.limit {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionLease {
                        slots: Arc::clone(self),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

struct ConnectionLease {
    slots: Arc<ConnectionSlots>,
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        self.slots.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Dev server that dispatches registered `Web.route` handlers and falls back
/// to static files for unmatched paths.
#[derive(Debug)]
pub struct WebDevServer {
    pub port: u16,
    shutdown_flag: Arc<AtomicBool>,
    listener_thread: Option<std::thread::JoinHandle<()>>,
    scoped_streams: Arc<ScopedUiStreams>,
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
    const MAX_CONCURRENT_CONNECTIONS: usize = 64;
    const SSE_PATH_PREFIX: &'static str = "/__nulang/sse/";
    const UI_SSE_PATH_PREFIX: &'static str = "/__nulang/ui-sse/";
    const SCOPED_UI_SSE_PATH_PREFIX: &'static str = "/__nulang/scoped-ui-sse/";

    pub fn scoped_ui_publisher(&self) -> ScopedUiPublisher {
        ScopedUiPublisher {
            streams: Arc::clone(&self.scoped_streams),
        }
    }

    /// Hosts must provide a verifier for authenticated session and room access.
    pub fn bind_with_ui_authorizer(
        port: u16,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<WebRoute>,
        authorizer: UiStreamAuthorizer,
    ) -> std::io::Result<Self> {
        let routes = routes
            .into_iter()
            .map(|route| RuntimeWebRoute { route, plan: None })
            .collect();
        Self::bind_runtime_with_ui_authorizer(
            port,
            static_dir,
            output_dir,
            routes,
            Some(authorizer),
        )
    }

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
        Self::bind_runtime_with_ui_authorizer(port, static_dir, output_dir, routes, None)
    }

    pub fn bind_runtime_with_ui_authorizer(
        port: u16,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<RuntimeWebRoute>,
        authorizer: Option<UiStreamAuthorizer>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let actual_port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let routes_clone = routes;
        let static_dir_clone = static_dir.clone();
        let output_dir_clone = output_dir.clone();
        let scoped_streams = Arc::new(ScopedUiStreams::default());
        let shared_scoped = Arc::clone(&scoped_streams);

        let handle = std::thread::Builder::new()
            .name("nulang-web-dev-listener".into())
            .spawn(move || {
                Self::listener_loop(
                    listener,
                    static_dir_clone,
                    output_dir_clone,
                    routes_clone,
                    shutdown_clone,
                    shared_scoped,
                    authorizer,
                );
            })?;

        Ok(WebDevServer {
            port: actual_port,
            shutdown_flag: shutdown,
            listener_thread: Some(handle),
            scoped_streams,
        })
    }

    fn listener_loop(
        listener: TcpListener,
        static_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        routes: Vec<RuntimeWebRoute>,
        shutdown: Arc<AtomicBool>,
        scoped_streams: Arc<ScopedUiStreams>,
        authorizer: Option<UiStreamAuthorizer>,
    ) {
        listener.set_nonblocking(true).ok();
        let slots = Arc::new(ConnectionSlots::new(Self::MAX_CONCURRENT_CONNECTIONS));
        // Route tables contain compiled modules; share them rather than
        // cloning the full vector for every incoming connection.
        let routes = Arc::new(routes);
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(lease) = slots.try_acquire() else {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                        HttpServerState::write_response(
                            &mut stream,
                            &HttpResponse {
                                status: 503,
                                headers: vec![
                                    ("Content-Type".into(), "text/plain".into()),
                                    ("Retry-After".into(), "1".into()),
                                ],
                                body: b"Server at connection capacity".to_vec(),
                            },
                        );
                        continue;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let static_dir = static_dir.clone();
                    let output_dir = output_dir.clone();
                    let routes = Arc::clone(&routes);
                    let shutdown = Arc::clone(&shutdown);
                    let scoped_streams = Arc::clone(&scoped_streams);
                    let authorizer = authorizer.clone();
                    // A failed spawn drops the closure and lease, returning capacity.
                    if let Err(error) = std::thread::Builder::new()
                        .name("nulang-web-dev-connection".into())
                        .spawn(move || {
                            let _lease = lease;
                            Self::handle_connection(
                                stream,
                                static_dir,
                                output_dir,
                                routes,
                                shutdown,
                                scoped_streams,
                                authorizer,
                            );
                        })
                    {
                        eprintln!("failed to spawn web connection worker: {error}");
                    }
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
        routes: Arc<Vec<RuntimeWebRoute>>,
        shutdown: Arc<AtomicBool>,
        scoped_streams: Arc<ScopedUiStreams>,
        authorizer: Option<UiStreamAuthorizer>,
    ) {
        let request = match read_http_request(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) => {
                HttpServerState::write_response(&mut stream, &error.response());
                return;
            }
        };
        let path = request.path.clone();

        let mut response = if path.starts_with(Self::SCOPED_UI_SSE_PATH_PREFIX) {
            let suffix = &path[Self::SCOPED_UI_SSE_PATH_PREFIX.len()..];
            let scope = suffix
                .split_once('/')
                .filter(|(app, room)| valid_scoped_ui_id(app) && valid_scoped_ui_id(room));
            // A valid local Host and Origin are necessary, but never sufficient
            // to authorize a private app/room.
            let allowed_local = stream
                .peer_addr()
                .ok()
                .is_some_and(|peer| ui_stream_local_request_allowed(&request, peer.ip()));
            let permitted = scope.as_ref().is_some_and(|(app, room)| {
                authorizer
                    .as_ref()
                    .is_some_and(|verify| verify(&request, app, room))
            });
            if !allowed_local || !permitted {
                HttpResponse {
                    status: 403,
                    headers: vec![
                        ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                        ("Cache-Control".into(), "no-store".into()),
                    ],
                    body: b"Scoped UI stream access denied".to_vec(),
                }
            } else if request.method != HttpMethod::Get {
                HttpResponse {
                    status: 405,
                    headers: vec![("Allow".into(), "GET".into())],
                    body: b"Method Not Allowed".to_vec(),
                }
            } else {
                let (app, room) = scope.expect("validated private room");
                Self::serve_scoped_ui_sse(
                    Arc::clone(&scoped_streams),
                    app,
                    room,
                    &request,
                    authorizer.as_ref().expect("authorized callback"),
                    &mut stream,
                    Arc::clone(&shutdown),
                );
                return;
            }
        } else if path.starts_with(Self::UI_SSE_PATH_PREFIX) {
            let room = &path[Self::UI_SSE_PATH_PREFIX.len()..];
            if !stream
                .peer_addr()
                .ok()
                .is_some_and(|peer| ui_stream_local_request_allowed(&request, peer.ip()))
            {
                HttpResponse {
                    status: 403,
                    headers: vec![
                        ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                        ("Cache-Control".into(), "no-store".into()),
                    ],
                    body: b"UI stream access denied".to_vec(),
                }
            } else if request.method != HttpMethod::Get {
                HttpResponse {
                    status: 405,
                    headers: vec![("Allow".into(), "GET".into())],
                    body: b"Method Not Allowed".to_vec(),
                }
            } else {
                Self::serve_ui_sse(room, &mut stream, shutdown.clone());
                return;
            }
        } else if path.starts_with(Self::SSE_PATH_PREFIX) {
            let room = &path[Self::SSE_PATH_PREFIX.len()..];
            Self::serve_sse(room, &mut stream, shutdown.clone());
            return;
        } else if let Some((route, params)) = routes
            .iter()
            .filter(|r| r.route.method == request.method)
            .find_map(|r| {
                crate::web::dispatch::match_route(r, &request.path).map(|params| (r, params))
            })
        {
            let captured =
                HttpRequestBindingInputs::capture(&request.path, &request.headers, &request.body);
            if let Some(response) = invalid_ui_action_response(&captured) {
                response
            } else {
                let values = captured.values(&params, &request.headers);
                let ctx = RequestContext {
                    request: request.clone(),
                    params: params.clone(),
                };
                let rendered =
                    with_request_context(ctx, || match render_direct_request(route, &values) {
                        Ok(Some(rendered)) => Ok(Some(rendered)),
                        Ok(None) => Ok(render_route_handler(
                            &route.route.handler_module,
                            route.route.handler_func_idx,
                            None,
                        )),
                        Err(error) => Err(error),
                    });
                match rendered {
                    Ok(Some(html)) => HttpResponse {
                        status: 200,
                        headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                        body: inject_client_runtime_script(&html).into_bytes(),
                    },
                    Ok(None) => HttpResponse {
                        status: 500,
                        headers: vec![("Content-Type".into(), "text/plain".into())],
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
                            headers: vec![("Content-Type".into(), "text/plain".into())],
                            body: b"Internal server error".to_vec(),
                        }
                    }
                }
            }
        } else {
            Self::serve_static(static_dir.as_deref(), output_dir.as_deref(), &request.path)
        };
        for cookie in take_response_cookies() {
            response.headers.push(("Set-Cookie".into(), cookie));
        }
        let middlewares = [Middleware::SecurityHeaders, Middleware::RequestLog];
        for m in &middlewares {
            m.apply(&request, &mut response);
        }
        HttpServerState::write_response(&mut stream, &response);
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

    fn serve_scoped_ui_sse(
        streams: Arc<ScopedUiStreams>,
        app: &str,
        room: &str,
        request: &HttpRequest,
        verify: &UiStreamAuthorizer,
        stream: &mut TcpStream,
        shutdown: Arc<AtomicBool>,
    ) {
        let Some((snapshot, subscriber)) = subscribe_scoped_ui(streams, app, room) else {
            HttpServerState::write_response(
                stream,
                &HttpResponse {
                    status: 503,
                    headers: vec![
                        ("Content-Type".into(), "text/plain".into()),
                        ("Cache-Control".into(), "no-store".into()),
                    ],
                    body: b"Scoped UI stream capacity exhausted".to_vec(),
                },
            );
            return;
        };
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        if !(verify)(request, app, room) || !subscriber.still_current() {
            return;
        }
        if stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store, no-transform\r\nX-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n\r\n"
        ).is_err() {
            return;
        }
        if Self::write_ui_sse_event(stream, &snapshot).is_err() {
            return;
        }
        let mut last_check = Instant::now();
        let mut last_heartbeat = Instant::now();
        while !shutdown.load(Ordering::Relaxed) {
            match subscriber.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(patch) => {
                    // Verify current membership before writing any next update.
                    if !(verify)(request, app, room)
                        || !matches!(
                            Self::write_scoped_ui_patch(&subscriber, stream, &patch),
                            Ok(true)
                        )
                    {
                        break;
                    }
                    last_check = Instant::now();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if last_check.elapsed() >= Duration::from_secs(1) {
                if !(verify)(request, app, room) || !subscriber.still_current() {
                    break;
                }
                last_check = Instant::now();
            }
            if last_heartbeat.elapsed() >= Duration::from_secs(15) {
                if stream.write_all(b": heartbeat\n\n").is_err() {
                    break;
                }
                last_heartbeat = Instant::now();
            }
        }
    }

    /// The exact wire-delivery boundary checks room ownership at delivery
    /// time, so queued messages from a canceled actor room cannot be written
    /// even when a new actor reuses the same application/room path.
    fn write_scoped_ui_patch<W: Write>(
        subscriber: &ScopedUiSubscriber,
        writer: &mut W,
        patch: &nulang_ui_protocol::UiHtmlStreamMessage,
    ) -> std::io::Result<bool> {
        if !subscriber.still_current() {
            return Ok(false);
        }
        Self::write_ui_sse_event(writer, patch)?;
        Ok(true)
    }

    fn serve_ui_sse(room: &str, stream: &mut TcpStream, shutdown: Arc<AtomicBool>) {
        let Some((snapshot, rx)) = ui_stream_subscribe(room) else {
            HttpServerState::write_response(
                stream,
                &HttpResponse {
                    status: 503,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: b"UI stream capacity exhausted or invalid room".to_vec(),
                },
            );
            return;
        };
        // Slow receivers are disconnected instead of blocking publishers.
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        if stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nX-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n\r\n"
        ).is_err() {
            return;
        }
        if Self::write_ui_sse_event(stream, &snapshot).is_err() {
            return;
        }
        let mut last_heartbeat = Instant::now();
        while !shutdown.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(patch) => {
                    if Self::write_ui_sse_event(stream, &patch).is_err() {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if last_heartbeat.elapsed() >= Duration::from_secs(15) {
                if stream.write_all(b": heartbeat\n\n").is_err() {
                    break;
                }
                last_heartbeat = Instant::now();
            }
        }
    }

    fn write_ui_sse_event<W: Write>(
        writer: &mut W,
        msg: &nulang_ui_protocol::UiHtmlStreamMessage,
    ) -> std::io::Result<()> {
        let event = match msg {
            nulang_ui_protocol::UiHtmlStreamMessage::Snapshot { .. } => "nulang-ui-snapshot",
            nulang_ui_protocol::UiHtmlStreamMessage::Patch { .. } => "nulang-ui-patch",
        };
        let json = serde_json::to_string(msg).map_err(std::io::Error::other)?;
        write!(
            writer,
            "event: {event}\nid: {}\ndata: {json}\n\n",
            msg.revision().0
        )
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
            .and_then(|root| contained_static_file(root, trimmed))
            .or_else(|| output_dir.and_then(|root| contained_static_file(root, trimmed)));

        if let Some((canonical, requested)) = file_path {
            if let Ok(data) = std::fs::read(&canonical) {
                let content_type = guess_content_type(&requested);
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

/// Resolve files only inside the configured static root. Resolve symlinks
/// before checking containment; do not trust the URL's path components.
/// Does not protect a concurrently mutated, hostile filesystem root.
fn contained_static_file(root: &Path, relative: &str) -> Option<(PathBuf, PathBuf)> {
    if !Path::new(relative)
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }

    let root = root.canonicalize().ok()?;
    let joined = root.join(relative);
    let candidate = if relative.is_empty() || joined.is_dir() {
        root.join("index.html")
    } else {
        joined
    };
    let canonical = candidate.canonicalize().ok()?;
    if canonical.starts_with(&root) && canonical.is_file() {
        Some((canonical, candidate))
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

    // A deterministic reader forces TCP-like fragmentation without sleeps.
    struct FixedChunkReader<'a> {
        remaining: &'a [u8],
        max_chunk: usize,
    }

    impl std::io::Read for FixedChunkReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.remaining.len().min(buf.len()).min(self.max_chunk);
            buf[..n].copy_from_slice(&self.remaining[..n]);
            self.remaining = &self.remaining[n..];
            Ok(n)
        }
    }

    fn chunks(input: &[u8], max_chunk: usize) -> FixedChunkReader<'_> {
        FixedChunkReader {
            remaining: input,
            max_chunk,
        }
    }

    // Fail closed on URL parent components and symlinks outside static roots.
    #[test]
    fn web_static_file_fallback_cannot_read_parent_directory() {
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = Cleanup(std::env::temp_dir().join(format!(
            "nulang-static-containment-{}-{suffix}",
            std::process::id()
        )));
        let public = fixture.0.join("public");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("hello.txt"), b"public").unwrap();
        std::fs::write(fixture.0.join("private.txt"), b"private").unwrap();

        let allowed = WebDevServer::serve_static(Some(&public), None, "/hello.txt");
        assert_eq!(allowed.status, 200);
        assert_eq!(allowed.body, b"public");

        let escaped = WebDevServer::serve_static(Some(&public), None, "/../private.txt");
        assert_eq!(escaped.status, 404, "parent traversal must fail closed");
        assert!(!escaped.body.windows(7).any(|part| part == b"private"));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../private.txt", public.join("shortcut.txt")).unwrap();
            let escaped = WebDevServer::serve_static(Some(&public), None, "/shortcut.txt");
            assert_eq!(escaped.status, 404, "symlink outside root must fail closed");

            // A public extension can differ from a safe in-root symlink target.
            std::fs::write(public.join("bundle-hash"), b"console.log('ok')").unwrap();
            std::os::unix::fs::symlink("bundle-hash", public.join("app.js")).unwrap();
            let script = WebDevServer::serve_static(Some(&public), None, "/app.js");
            assert_eq!(script.status, 200);
            assert_eq!(script.body, b"console.log('ok')");
            assert!(script.headers.iter().any(|(name, value)| {
                name == "Content-Type" && value == "application/javascript; charset=utf-8"
            }));
        }
    }

    #[test]
    fn http_request_reassembles_fragmented_headers_and_exact_body() {
        let raw = b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello";
        let request = read_http_request(&mut chunks(raw, 3)).unwrap().unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/submit");
        assert_eq!(request.body, b"hello");
    }

    #[test]
    fn http_request_rejects_truncated_body_instead_of_dispatching() {
        let raw = b"POST /submit HTTP/1.1\r\nContent-Length: 9\r\n\r\nhello";
        assert_eq!(
            read_http_request(&mut chunks(raw, 2)).unwrap_err(),
            HttpReadError::BadRequest
        );
    }

    #[test]
    fn http_request_rejects_ambiguous_or_invalid_lengths() {
        for raw in [
            &b"POST / HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\nok"[..],
            &b"POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n"[..],
            &b"POST / HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\nok"[..],
        ] {
            assert_eq!(
                read_http_request(&mut chunks(raw, 7)).unwrap_err(),
                HttpReadError::BadRequest
            );
        }
    }

    #[test]
    fn http_request_rejects_unknown_methods_instead_of_treating_as_get() {
        assert_eq!(
            read_http_request(&mut chunks(b"BREW / HTTP/1.1\r\n\r\n", 32)).unwrap_err(),
            HttpReadError::NotImplemented
        );
    }

    #[test]
    fn http_request_rejects_unsupported_transfer_encoding() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(
            read_http_request(&mut chunks(raw, 8)).unwrap_err(),
            HttpReadError::NotImplemented
        );
    }

    #[test]
    fn http_request_rejects_oversized_payload_from_declared_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 1048577\r\n\r\n";
        assert_eq!(
            read_http_request(&mut chunks(raw, 32)).unwrap_err(),
            HttpReadError::PayloadTooLarge
        );
    }

    #[test]
    fn http_request_bounds_headers_even_without_terminator() {
        let mut raw = b"GET / HTTP/1.1\r\nX-Padding: ".to_vec();
        raw.extend(vec![b'a'; 17000]);
        assert_eq!(
            read_http_request(&mut chunks(&raw, 900)).unwrap_err(),
            HttpReadError::HeadersTooLarge
        );
    }

    #[test]
    fn http_request_rejects_expect_continue_without_blocking_on_body() {
        let raw = b"POST / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n";
        assert_eq!(
            read_http_request(&mut chunks(raw, 8)).unwrap_err(),
            HttpReadError::ExpectationFailed
        );
    }

    #[test]
    fn http_request_does_not_consume_pipelined_bytes_as_body() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nokGET / HTTP/1.1\r\n\r\n";
        let request = read_http_request(&mut chunks(raw, 128)).unwrap().unwrap();
        assert_eq!(request.body, b"ok");
    }

    #[test]
    fn test_web_connection_slots_enforce_limit_and_release_on_drop() {
        let slots = Arc::new(ConnectionSlots::new(2));
        let first = slots.try_acquire().expect("first worker capacity");
        let second = slots.try_acquire().expect("second worker capacity");
        assert!(
            slots.try_acquire().is_none(),
            "third connection must not spawn"
        );
        assert_eq!(slots.active.load(Ordering::Relaxed), 2);
        drop(first);
        assert!(
            slots.try_acquire().is_some(),
            "capacity returns on worker exit"
        );
        drop(second);
    }

    #[test]
    fn test_web_connection_slots_are_bounded_under_concurrent_admission() {
        let slots = Arc::new(ConnectionSlots::new(3));
        let start = Arc::new(std::sync::Barrier::new(16));
        let hold = Arc::new(std::sync::Barrier::new(16));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let slots = slots.clone();
            let start = start.clone();
            let hold = hold.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let lease = slots.try_acquire();
                hold.wait();
                lease.is_some()
            }));
        }
        let accepted = threads
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(accepted, 3);
        assert_eq!(slots.active.load(Ordering::Relaxed), 0);
    }

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

    fn sse_tcp_connect(
        port: u16,
        room: &str,
        extra_headers: &str,
    ) -> std::io::BufReader<TcpStream> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET /__nulang/ui-sse/{room} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n{extra_headers}\r\n"
        ).unwrap();
        std::io::BufReader::new(stream)
    }

    fn sse_tcp_frame(reader: &mut impl std::io::BufRead) -> String {
        let mut frame = String::new();
        for _ in 0..128 {
            let mut line = String::new();
            let count = reader.read_line(&mut line).unwrap();
            assert!(count > 0, "SSE stream closed unexpectedly");
            if line == "\r\n" || line == "\n" {
                return frame;
            }
            frame.push_str(&line);
        }
        panic!("SSE frame exceeded the configured line limit");
    }

    fn scoped_tcp_connect(
        port: u16,
        app: &str,
        room: &str,
        extra_headers: &str,
    ) -> std::io::BufReader<TcpStream> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET /__nulang/scoped-ui-sse/{app}/{room} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n{extra_headers}\r\n"
        ).unwrap();
        std::io::BufReader::new(stream)
    }

    #[test]
    fn scoped_ui_sse_denies_when_host_has_no_authorizer() {
        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let mut client = scoped_tcp_connect(server.port, "billing", "invoices", "");
        assert!(sse_tcp_frame(&mut client).starts_with("HTTP/1.1 403 Forbidden"));
    }

    #[test]
    fn scoped_ui_sse_requires_verified_membership_and_separates_instances() {
        let authorize: UiStreamAuthorizer = Arc::new(|request, app, room| {
            app == "billing"
                && room == "invoices"
                && request.headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("cookie") && value == "session=test-verified"
                })
        });
        let server =
            WebDevServer::bind_with_ui_authorizer(0, None, None, vec![], Arc::clone(&authorize))
                .unwrap();
        let publisher = server.scoped_ui_publisher();
        assert!(publisher.publish(
            "billing",
            "invoices",
            "#status",
            "<div id=\"status\">private</div>"
        ));
        assert!(!publisher.publish("billing/other", "invoices", "#status", "bad"));
        assert!(!publisher.publish("billing", "invoices", "body", "bad"));

        let mut absent = scoped_tcp_connect(server.port, "billing", "invoices", "");
        assert!(sse_tcp_frame(&mut absent).starts_with("HTTP/1.1 403 Forbidden"));
        let mut wrong_app = scoped_tcp_connect(
            server.port,
            "other",
            "invoices",
            "Cookie: session=test-verified\r\n",
        );
        assert!(sse_tcp_frame(&mut wrong_app).starts_with("HTTP/1.1 403 Forbidden"));
        let mut wrong_room = scoped_tcp_connect(
            server.port,
            "billing",
            "other",
            "Cookie: session=test-verified\r\n",
        );
        assert!(sse_tcp_frame(&mut wrong_room).starts_with("HTTP/1.1 403 Forbidden"));
        let mut invalid = scoped_tcp_connect(
            server.port,
            "billing",
            "invoices/extra",
            "Cookie: session=test-verified\r\n",
        );
        assert!(sse_tcp_frame(&mut invalid).starts_with("HTTP/1.1 403 Forbidden"));

        let mut allowed = scoped_tcp_connect(
            server.port,
            "billing",
            "invoices",
            "Cookie: session=test-verified\r\n",
        );
        assert!(sse_tcp_frame(&mut allowed).starts_with("HTTP/1.1 200 OK"));
        let snapshot = sse_tcp_frame(&mut allowed);
        assert!(snapshot.contains("event: nulang-ui-snapshot"), "{snapshot}");
        assert!(snapshot.contains("private"), "{snapshot}");
        assert!(snapshot.contains("billing/invoices"), "{snapshot}");
        assert!(publisher.publish(
            "billing",
            "invoices",
            "#status",
            "<div id=\"status\">updated</div>"
        ));
        let patch = sse_tcp_frame(&mut allowed);
        assert!(patch.contains("event: nulang-ui-patch"), "{patch}");
        assert!(patch.contains("updated"), "{patch}");
        drop(allowed);

        // A second, independently bound app host must not reuse the room.
        let second =
            WebDevServer::bind_with_ui_authorizer(0, None, None, vec![], authorize).unwrap();
        let mut fresh = scoped_tcp_connect(
            second.port,
            "billing",
            "invoices",
            "Cookie: session=test-verified\r\n",
        );
        assert!(sse_tcp_frame(&mut fresh).starts_with("HTTP/1.1 200 OK"));
        let isolated = sse_tcp_frame(&mut fresh);
        assert!(isolated.contains(r#""elements":[]"#), "{isolated}");
        drop(fresh);

        // Legacy public endpoint is a separate in-memory registry.
        let mut public = sse_tcp_connect(server.port, "billing", "");
        assert!(sse_tcp_frame(&mut public).starts_with("HTTP/1.1 200 OK"));
        let public_snapshot = sse_tcp_frame(&mut public);
        assert!(!public_snapshot.contains("private"), "{public_snapshot}");
        drop(public);
    }

    #[test]
    fn actor_scoped_ui_mailbox_delivery_cancels_on_exit() {
        use crate::runtime::{ExitReason, Runtime};
        use crate::vm::Value;
        let mut runtime = Runtime::new();
        let sender = runtime.spawn_actor(Box::new(|| vec![]));
        let owner = runtime.spawn_actor(Box::new(|| vec![]));
        runtime
            .actors
            .get_mut(&owner)
            .unwrap()
            .register_behavior("render", |_actor, _args| {});
        let verifier: UiStreamAuthorizer =
            Arc::new(|_request, app, room| app == "billing" && room == "invoices");
        let server =
            WebDevServer::bind_with_ui_authorizer(0, None, None, vec![], verifier).unwrap();
        let publisher = server.scoped_ui_publisher();
        let binding = publisher
            .bind_actor(&mut runtime, owner, "billing", "invoices")
            .expect("alive actor can own private stream");
        assert!(
            publisher
                .bind_actor(&mut runtime, sender, "billing", "invoices")
                .is_none(),
            "other actors cannot seize an owned room"
        );
        assert!(
            publisher
                .bind_actor(&mut runtime, 999999, "billing", "forged")
                .is_none(),
            "unknown actors cannot own rooms"
        );

        let mut client = scoped_tcp_connect(server.port, "billing", "invoices", "");
        assert!(sse_tcp_frame(&mut client).starts_with("HTTP/1.1 200 OK"));
        assert!(sse_tcp_frame(&mut client).contains("nulang-ui-snapshot"));

        // The actual Nulang mailbox carries the event across actors.
        runtime.current_actor = Some(sender);
        runtime.send_message(owner, "render", &[Value::int(73)]);
        runtime.current_actor = None;
        let message = runtime.actors.get_mut(&owner).unwrap().receive().unwrap();
        assert_eq!(message.sender, sender);
        assert_eq!(message.payload.as_slice(), &[Value::int(73)]);
        runtime.current_actor = Some(sender);
        assert!(!binding.publish(&runtime, sender, "#status", "<p>spoof</p>"));
        assert!(
            !publisher.publish("billing", "invoices", "#status", "<p>bypass</p>"),
            "plain host publisher must not overwrite an actor-owned room"
        );
        runtime.current_actor = Some(owner);
        assert!(binding.publish(&runtime, owner, "#status", "<p>73</p>"));
        let patch = sse_tcp_frame(&mut client);
        assert!(patch.contains("event: nulang-ui-patch"), "{patch}");
        assert!(patch.contains("<p>73</p>"), "{patch}");

        // A different runtime can have the same local numeric actor ID;
        // its messages must not acquire the original runtime's capability.
        let mut imposter = Runtime::new();
        imposter.current_actor = Some(owner);
        imposter
            .actors
            .insert(owner, super::super::Actor::new(owner, "imposter", 8));
        assert!(!binding.publish(&imposter, owner, "#status", "<p>imposter</p>"));
        // Hold a real subscriber's patch queue while the actor exits.
        // The network worker may legally write a patch BEFORE exit, so
        // exercise the exact post-exit write boundary deterministically.
        let (_snapshot, paused) =
            subscribe_scoped_ui(Arc::clone(&publisher.streams), "billing", "invoices")
                .expect("independent bounded private subscriber");
        assert!(binding.publish(&runtime, owner, "#status", "<p>queued</p>"));
        runtime.current_actor = None;
        runtime.exit_actor(owner, ExitReason::Normal);
        assert!(!binding.publish(&runtime, owner, "#status", "<p>stale</p>"));
        let queued = paused.rx.try_recv().expect("queued while actor was alive");
        assert!(
            !paused.still_current(),
            "terminated actor lease is no longer current"
        );
        let mut writer = Vec::new();
        assert!(!WebDevServer::write_scoped_ui_patch(&paused, &mut writer, &queued).unwrap());
        assert!(
            writer.is_empty(),
            "old queued HTML must never cross a canceled room boundary"
        );
        assert!(
            matches!(
                paused.rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "actor exit must disconnect its remaining subscribers"
        );
        drop(client);
    }

    #[test]
    fn actor_scoped_ui_cancel_is_idempotent_and_releases_room() {
        use crate::runtime::Runtime;
        let mut runtime = Runtime::new();
        let actor = runtime.spawn_actor(Box::new(|| vec![]));
        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let publisher = server.scoped_ui_publisher();
        let binding = publisher
            .bind_actor(&mut runtime, actor, "app", "room")
            .unwrap();
        runtime.current_actor = Some(actor);
        assert!(!binding.publish(&runtime, actor + 1, "#status", "<p>other</p>"));
        assert!(binding.publish(&runtime, actor, "#status", "<p>one</p>"));
        binding.cancel();
        binding.cancel();
        assert!(!binding.publish(&runtime, actor, "#status", "<p>after</p>"));
        let replacement = publisher
            .bind_actor(&mut runtime, actor, "app", "room")
            .expect("cancellation frees the room");
        assert!(replacement.publish(&runtime, actor, "#status", "<p>new</p>"));
        drop(replacement);
        assert!(
            publisher
                .bind_actor(&mut runtime, actor, "app", "room")
                .is_some(),
            "dropping a binding must automatically cancel"
        );
    }

    #[test]
    fn scoped_ui_sse_revocation_stops_live_delivery() {
        let active = Arc::new(AtomicBool::new(true));
        let control = Arc::clone(&active);
        let auth: UiStreamAuthorizer = Arc::new(move |_request, app, room| {
            control.load(Ordering::SeqCst) && app == "billing" && room == "invoices"
        });
        let server = WebDevServer::bind_with_ui_authorizer(0, None, None, vec![], auth).unwrap();
        let publisher = server.scoped_ui_publisher();
        let mut client = scoped_tcp_connect(server.port, "billing", "invoices", "");
        assert!(sse_tcp_frame(&mut client).starts_with("HTTP/1.1 200 OK"));
        assert!(sse_tcp_frame(&mut client).contains("event: nulang-ui-snapshot"));
        active.store(false, Ordering::SeqCst);
        assert!(publisher.publish("billing", "invoices", "#status", "<p>must-not-leak</p>"));
        let mut remaining = String::new();
        let bytes = std::io::BufRead::read_line(&mut client, &mut remaining).unwrap();
        assert_eq!(
            bytes, 0,
            "revoked SSE must close before writing: {remaining}"
        );
        assert!(!remaining.contains("must-not-leak"));
    }

    #[test]
    fn ui_stream_tcp_snapshot_patch_and_reconnect() {
        let room = format!("tcp-ui-{}", std::process::id());
        assert!(ui_stream_publish(
            &room,
            "#status",
            "<p id=\"status\">one</p>"
        ));
        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let mut first = sse_tcp_connect(server.port, &room, "");
        let headers = sse_tcp_frame(&mut first);
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
        assert!(
            headers.contains("Content-Type: text/event-stream"),
            "{headers}"
        );
        assert!(
            headers.contains("X-Content-Type-Options: nosniff"),
            "{headers}"
        );

        let snapshot = sse_tcp_frame(&mut first);
        assert!(snapshot.contains("event: nulang-ui-snapshot"), "{snapshot}");
        assert!(snapshot.contains("id: 1"), "{snapshot}");
        assert!(snapshot.contains(r#""revision":"1""#), "{snapshot}");

        assert!(ui_stream_publish(
            &room,
            "#status",
            "<p id=\"status\">two</p>"
        ));
        let patch = sse_tcp_frame(&mut first);
        assert!(patch.contains("event: nulang-ui-patch"), "{patch}");
        assert!(patch.contains("id: 2"), "{patch}");
        assert!(patch.contains(r#""base_revision":"1""#), "{patch}");
        assert!(patch.contains("two"), "{patch}");

        drop(first);
        let mut reconnect = sse_tcp_connect(server.port, &room, "Last-Event-ID: 1\r\n");
        assert!(sse_tcp_frame(&mut reconnect).starts_with("HTTP/1.1 200 OK"));
        let latest = sse_tcp_frame(&mut reconnect);
        assert!(latest.contains("event: nulang-ui-snapshot"), "{latest}");
        assert!(latest.contains("id: 2"), "{latest}");
        assert!(latest.contains("two"), "{latest}");
        drop(reconnect);
        drop(server);
    }

    #[test]
    fn ui_stream_tcp_rejects_cross_origin_before_subscribing() {
        let room = format!("tcp-origin-{}", std::process::id());
        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let mut cross_site = sse_tcp_connect(
            server.port,
            &room,
            "Origin: https://evil.example\r\nSec-Fetch-Site: cross-site\r\n",
        );
        let headers = sse_tcp_frame(&mut cross_site);
        assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
        let count = ui_stream_rooms()
            .lock()
            .unwrap()
            .get(&room)
            .map(|room| room.listeners.len())
            .unwrap_or(0);
        assert_eq!(count, 0, "rejected origins must never subscribe");
        drop(cross_site);
        drop(server);
    }

    #[test]
    fn ui_stream_security_policy_rejects_nonloopback_and_dns_rebinding() {
        use std::net::{IpAddr, Ipv4Addr};
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let remote = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        let mut request = HttpRequest {
            method: HttpMethod::Get,
            path: "/__nulang/ui-sse/tasks".into(),
            headers: vec![("Host".into(), "localhost:1234".into())],
            body: vec![],
        };
        assert!(ui_stream_local_request_allowed(&request, loopback));
        assert!(!ui_stream_local_request_allowed(&request, remote));
        request.headers[0].1 = "attacker.example:1234".into();
        assert!(!ui_stream_local_request_allowed(&request, loopback));
        request.headers[0].1 = "localhost.evil.example:1234".into();
        assert!(!ui_stream_local_request_allowed(&request, loopback));
        request.headers[0].1 = "127.0.0.1:1234".into();
        request
            .headers
            .push(("Origin".into(), "http://127.0.0.1:1234".into()));
        request
            .headers
            .push(("Sec-Fetch-Site".into(), "same-origin".into()));
        assert!(ui_stream_local_request_allowed(&request, loopback));
        request.headers[2].1 = "cross-site".into();
        assert!(!ui_stream_local_request_allowed(&request, loopback));
        request.headers[2].1 = "same-origin".into();
        request
            .headers
            .push(("Origin".into(), "http://127.0.0.1:1234".into()));
        assert!(!ui_stream_local_request_allowed(&request, loopback));
        request.headers.pop();
        request.headers[1].1 = "null".into();
        assert!(!ui_stream_local_request_allowed(&request, loopback));
    }

    #[test]
    fn ui_stream_tcp_allows_same_origin_and_rejects_cross_site_fetch() {
        let room = format!("tcp-guard-{}", std::process::id());
        let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
        let headers = format!(
            "Origin: http://127.0.0.1:{}\r\nSec-Fetch-Site: same-origin\r\n",
            server.port
        );
        let mut trusted = sse_tcp_connect(server.port, &room, &headers);
        assert!(sse_tcp_frame(&mut trusted).starts_with("HTTP/1.1 200 OK"));
        assert!(sse_tcp_frame(&mut trusted).contains("event: nulang-ui-snapshot"));
        drop(trusted);

        let mut cross_site = sse_tcp_connect(server.port, &room, "Sec-Fetch-Site: cross-site\r\n");
        let headers = sse_tcp_frame(&mut cross_site);
        assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
        drop(cross_site);

        let mut rebinding = sse_tcp_connect(server.port, &room, "Host: attacker.example\r\n");
        let headers = sse_tcp_frame(&mut rebinding);
        assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
        drop(rebinding);
        drop(server);
    }

    #[test]
    fn ui_stream_starts_with_snapshot_and_revises_patches() {
        let room = format!("ui-stream-replay-{}", std::process::id());
        assert!(ui_stream_publish(&room, "#count", "<b id=\"count\">1</b>"));
        let (snapshot, rx) = ui_stream_subscribe(&room).expect("subscription admitted");
        assert_eq!(snapshot.revision(), nulang_ui_protocol::Revision(1));
        assert!(matches!(
            snapshot,
            nulang_ui_protocol::UiHtmlStreamMessage::Snapshot { .. }
        ));
        assert!(ui_stream_publish(&room, "#count", "<b id=\"count\">2</b>"));
        let patch = rx.try_recv().unwrap();
        assert!(matches!(
            patch,
            nulang_ui_protocol::UiHtmlStreamMessage::Patch { .. }
        ));
        assert_eq!(patch.revision(), nulang_ui_protocol::Revision(2));
        assert!(patch.validate().is_ok());

        let (snapshot, _rx) = ui_stream_subscribe(&room).unwrap();
        assert_eq!(snapshot.revision(), nulang_ui_protocol::Revision(2));
    }

    #[test]
    fn ui_stream_bounds_and_drops_slow_subscribers() {
        let room = format!("ui-stream-bounds-{}", std::process::id());
        let (_snapshot, rx) = ui_stream_subscribe(&room).unwrap();
        assert!(!ui_stream_publish(&room, "body", "invalid selector"));
        assert!(!ui_stream_publish(&room, "#count", &"x".repeat(65537)));
        for n in 0..32 {
            assert!(ui_stream_publish(&room, &format!("#item-{n}"), "ok"));
        }
        assert!(!ui_stream_publish(&room, "#item-overflow", "ok"));
        for _ in 0..32 {
            assert!(ui_stream_publish(&room, "#item-0", "ok"));
        }
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert!(
            received <= UI_STREAM_CHANNEL_DEPTH,
            "queue must stay bounded"
        );
    }

    #[test]
    fn ui_stream_sse_wire_frame_includes_typed_snapshot_and_revision() {
        let snapshot = nulang_ui_protocol::UiHtmlStreamMessage::snapshot(
            nulang_ui_protocol::DocumentId::from("room"),
            nulang_ui_protocol::Revision(42),
            vec![nulang_ui_protocol::UiHtmlElement {
                target: "#items".into(),
                html: "<b>hi\nthere</b>".into(),
            }],
        );
        let mut output = Vec::new();
        WebDevServer::write_ui_sse_event(&mut output, &snapshot).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("event: nulang-ui-snapshot\nid: 42\ndata: "));
        assert!(text.ends_with("\n\n"));
        assert!(text.contains("\"nulang-ui-html/1\""));
        assert!(text.contains("\"revision\":\"42\""));
    }

    #[test]
    fn ui_stream_unsubscribes_on_drop() {
        let room = format!("ui-stream-disconnect-{}", std::process::id());
        let (snapshot, receiver) = ui_stream_subscribe(&room).unwrap();
        assert_eq!(snapshot.revision(), nulang_ui_protocol::Revision(0));
        assert_eq!(ui_stream_rooms().lock().unwrap()[&room].listeners.len(), 1);
        drop(receiver);
        assert_eq!(ui_stream_rooms().lock().unwrap()[&room].listeners.len(), 0);
    }

    #[test]
    fn test_realtime_broadcast_history() {
        let room = "test-room";
        realtime_broadcast(room, "hello");
        let (history, _rx) = realtime_subscribe(room);
        assert_eq!(history, vec!["hello"]);
    }
}
