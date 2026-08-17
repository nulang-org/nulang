//! Standard-library inventory.
//!
//! This module is an inventory/documentation layer only: it does not
//! implement any behavior. It records every built-in effect operation and
//! function that is currently wired into the VM and runtime, so tools
//! (REPL, LSP, docs generators) have a single source of truth for what a
//! `perform Effect.op(...)` call resolves to when no user handler is
//! installed.
//!
//! ## Standard library modules (`.nula` files in `src/stdlib/`)
//!
//! | Module | File | Description |
//! |--------|------|-------------|
//! | `std.core` | `core.nula` | Core types (`Option[T]`, `Result[T, E]`) and combinators (auto-loaded). |
//! | `std.math` | `math.nula` | Math functions: `abs`, `min`, `max`, `clamp`, `pow`, `factorial`, `gcd`, `sqrt`. |
//! | `std.list` | `list.nula` | Functional list combinators: `map`, `filter`, `fold`, `append`, `reverse`, `sort`, etc. |
//! | `std.string` | `string.nula` | String operations: `trim`, `split`, `join`, `replace`, `to_upper`, `to_lower`, etc. |
//! | `std.map` | `map.nula` | Int→Int key-value map via sorted arrays: `insert`, `get`, `remove`, `contains`. |
//! | `std.set` | `set.nula` | Int set via sorted arrays: `insert`, `contains`, `remove`. |
//! | `std.result` | `result.nula` | Extra Result combinators: `unwrap`, `map`, `is_ok`, `is_err`. |
//! | `std.option` | `option.nula` | Extra Option combinators: `unwrap`, `map`, `is_some`, `is_none`. |
//! | `std.datetime` | `datetime.nula` | DateTime record type and operations: `now` (stub), `new`, `is_valid`. |
//! | `std.http` | `http.nula` | HTTP client via built-in `Http` effect: `get`, `post`. |
//! | `std.fs` | `fs.nula` | Filesystem I/O via built-in `FS` effect: `read`, `write`, `append`, `exists`. |
//! | `std.json` | `json.nula` | JSON parsing and serialization: `parse`, `stringify`, field accessors. |
//! | `std.test` | `test.nula` | Testing primitives: `assert_eq`, `assert_true`, `assert_false`, `fail`. |
//!
//! The wiring itself lives elsewhere:

use crate::types::Span;
use crate::types::{NuError, NuResult};

// ---------------------------------------------------------------------------
// BuiltinOp: one built-in effect operation
// ---------------------------------------------------------------------------

/// Where a built-in operation is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplSite {
    /// Handled by `VM::perform_builtin_effect` in the standalone VM
    /// (actor-free scripts); no runtime required.
    StandaloneVm,
    /// Handled by a runtime host callback (`ActorVmCallbacks`); requires
    /// the actor runtime and, for `Timer.sleep`, a workflow actor.
    RuntimeHost,
}

/// A single built-in effect operation wired into the VM/runtime.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinOp {
    /// Fully-qualified name as dispatched by the VM, e.g. `"IO.print"`.
    pub name: &'static str,
    /// Effect the operation belongs to, e.g. `"IO"`.
    pub effect: &'static str,
    /// Operation name within the effect, e.g. `"print"`.
    pub op: &'static str,
    /// Human-readable signature, e.g. `"print(msg: String) -> Unit"`.
    pub signature: &'static str,
    /// Where the operation is implemented.
    pub implemented_in: ImplSite,
    /// One-line description of the behavior.
    pub description: &'static str,
}

// ---------------------------------------------------------------------------
// StdLib: registry of built-in operations
// ---------------------------------------------------------------------------

/// Registry of every built-in effect operation currently wired into the
/// VM and runtime.
///
/// The registry is static: it mirrors the dispatch sites in `vm.rs` and
/// `runtime/mod.rs` and is updated by hand when a new built-in is wired.
pub struct StdLib {
    ops: Vec<BuiltinOp>,
}

impl StdLib {
    /// Build the registry with all currently wired built-ins.
    pub fn new() -> Self {
        StdLib {
            ops: vec![
                BuiltinOp {
                    name: "IO.print",
                    effect: "IO",
                    op: "print",
                    signature: "print(msg: String) -> Unit",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Write the argument to stdout, followed by a newline.",
                },
                BuiltinOp {
                    name: "IO.println",
                    effect: "IO",
                    op: "println",
                    signature: "println(msg: String) -> Unit",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Alias of `IO.print`; writes the argument to stdout with a newline.",
                },
                BuiltinOp {
                    name: "IO.read",
                    effect: "IO",
                    op: "read",
                    signature: "read() -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Read one line from stdin; returns the line without the trailing newline.",
                },
                BuiltinOp { name: "IO.log", effect: "IO", op: "log", signature: "log(level: String, message: String) -> Unit", implemented_in: ImplSite::StandaloneVm, description: "Log a message at the given level to stderr.", },
                BuiltinOp { name: "IO.log_error", effect: "IO", op: "log_error", signature: "log_error(message: String) -> Unit", implemented_in: ImplSite::StandaloneVm, description: "Log an error message to stderr.", },
                BuiltinOp { name: "Debug.inspect", effect: "Debug", op: "inspect", signature: "inspect(label: String, value: a) -> a", implemented_in: ImplSite::StandaloneVm, description: "Print a labeled value to stderr and return it unchanged.", },
                BuiltinOp {
                    name: "FS.read",
                    effect: "FS",
                    op: "read",
                    signature: "read(path: String) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Read the entire contents of a file into a string; returns nil on error.",
                },
                BuiltinOp {
                    name: "FS.write",
                    effect: "FS",
                    op: "write",
                    signature: "write(path: String, content: String) -> Unit",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Write a string to a file, overwriting any existing content; returns nil on error.",
                },
                BuiltinOp {
                    name: "FS.append",
                    effect: "FS",
                    op: "append",
                    signature: "append(path: String, content: String) -> Unit",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Append a string to the end of a file, creating it if it does not exist; returns nil on error.",
                },
                BuiltinOp {
                    name: "FS.exists",
                    effect: "FS",
                    op: "exists",
                    signature: "exists(path: String) -> Bool",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Check whether a file or directory exists at the given path.",
                },
                BuiltinOp {
                    name: "Array.length",
                    effect: "Array",
                    op: "length",
                    signature: "length(arr: [T]) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the number of elements in the array.",
                },
                BuiltinOp {
                    name: "StrBuilder.new",
                    effect: "StrBuilder",
                    op: "new",
                    signature: "new() -> StrBuilder",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Create an empty mutable string builder.",
                },
                BuiltinOp {
                    name: "StrBuilder.push",
                    effect: "StrBuilder",
                    op: "push",
                    signature: "push(b: StrBuilder, s: String) -> StrBuilder",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Append s to the builder (amortized O(1) with capacity doubling); rebind the returned pointer.",
                },
                BuiltinOp {
                    name: "StrBuilder.to_string",
                    effect: "StrBuilder",
                    op: "to_string",
                    signature: "to_string(b: StrBuilder) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Materialize the builder contents as an immutable string.",
                },
                BuiltinOp {
                    name: "StrBuilder.len",
                    effect: "StrBuilder",
                    op: "len",
                    signature: "len(b: StrBuilder) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the number of bytes currently in the builder.",
                },
                BuiltinOp {
                    name: "StrBuilder.reset",
                    effect: "StrBuilder",
                    op: "reset",
                    signature: "reset(b: StrBuilder) -> StrBuilder",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Clear the builder contents (capacity is retained).",
                },
                BuiltinOp {
                    name: "Map.new",
                    effect: "Map",
                    op: "new",
                    signature: "new() -> Map",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Create an empty mutable hash map (open addressing).",
                },
                BuiltinOp {
                    name: "Map.insert",
                    effect: "Map",
                    op: "insert",
                    signature: "insert(m: Map, k: T, v: U) -> Map",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Insert key k with value v (overwrites existing); may grow and return a new pointer.",
                },
                BuiltinOp {
                    name: "Map.get",
                    effect: "Map",
                    op: "get",
                    signature: "get(m: Map, k: T) -> U",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Look up key k; returns nil when absent. String keys compare by content.",
                },
                BuiltinOp {
                    name: "Map.contains",
                    effect: "Map",
                    op: "contains",
                    signature: "contains(m: Map, k: T) -> Bool",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "True when key k is present.",
                },
                BuiltinOp {
                    name: "Map.remove",
                    effect: "Map",
                    op: "remove",
                    signature: "remove(m: Map, k: T) -> Map",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Remove key k if present (marks a tombstone).",
                },
                BuiltinOp {
                    name: "Map.size",
                    effect: "Map",
                    op: "size",
                    signature: "size(m: Map) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the number of live key-value pairs.",
                },
                BuiltinOp {
                    name: "Array.push",
                    effect: "Array",
                    op: "push",
                    signature: "push(arr: [T], elem: T) -> [T]",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return a new array with elem appended to the end (value semantics).",
                },
                BuiltinOp {
                    name: "Array.new",
                    effect: "Array",
                    op: "new",
                    signature: "new(n: Int, init: T) -> [T]",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Create a new array of n copies of init.",
                },
                BuiltinOp {
                    name: "Array.set",
                    effect: "Array",
                    op: "set",
                    signature: "set(arr: [T], idx: Int, val: T) -> [T]",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return a new array with index idx replaced by val (value semantics).",
                },
                BuiltinOp {
                    name: "Array.slice",
                    effect: "Array",
                    op: "slice",
                    signature: "slice(arr: [T], start: Int, end: Int) -> [T]",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return a new array containing elements from start (inclusive) to end (exclusive).",
                },
                BuiltinOp {
                    name: "Test.assert",
                    effect: "Test",
                    op: "assert",
                    signature: "assert(cond: Bool, msg: String) -> Unit ! {Test}",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Assert a condition is true; raises a runtime error with the given message on failure.",
                },
                BuiltinOp {
                    name: "Test.assert_eq",
                    effect: "Test",
                    op: "assert_eq",
                    signature: "assert_eq(a: Int, b: Int) -> Unit ! {Test}",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Assert two integers are equal; raises a runtime error on failure.",
                },
                BuiltinOp {
                    name: "Test.assert_true",
                    effect: "Test",
                    op: "assert_true",
                    signature: "assert_true(cond: Bool) -> Unit ! {Test}",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Assert a condition is true; raises a runtime error on failure.",
                },
                BuiltinOp {
                    name: "Int.to_string",
                    effect: "Int",
                    op: "to_string",
                    signature: "to_string(value: Int) -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Convert an integer to its string representation.",
                },
                BuiltinOp {
                    name: "Int.to_float",
                    effect: "Int",
                    op: "to_float",
                    signature: "to_float(value: Int) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Convert an integer to a floating-point number.",
                },
                BuiltinOp {
                    name: "Int.to_hex",
                    effect: "Int",
                    op: "to_hex",
                    signature: "to_hex(n: Int) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Format an integer as a hexadecimal string (lowercase, no prefix).",
                },
                BuiltinOp {
                    name: "Int.to_binary",
                    effect: "Int",
                    op: "to_binary",
                    signature: "to_binary(n: Int) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Format an integer as a binary string.",
                },
                BuiltinOp {
                    name: "Float.to_int",
                    effect: "Float",
                    op: "to_int",
                    signature: "to_int(value: Float) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Convert a float to an integer by truncation toward zero.",
                },
                BuiltinOp {
                    name: "Float.to_string",
                    effect: "Float",
                    op: "to_string",
                    signature: "to_string(value: Float) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Format a float as a string.",
                },
                BuiltinOp {
                    name: "Float.sin",
                    effect: "Float",
                    op: "sin",
                    signature: "sin(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the sine of a float (radians).",
                },
                BuiltinOp {
                    name: "Float.cos",
                    effect: "Float",
                    op: "cos",
                    signature: "cos(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the cosine of a float (radians).",
                },
                BuiltinOp {
                    name: "Float.sqrt",
                    effect: "Float",
                    op: "sqrt",
                    signature: "sqrt(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the square root of a float. Returns nil for negative input.",
                },
                BuiltinOp {
                    name: "Float.tan",
                    effect: "Float",
                    op: "tan",
                    signature: "tan(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the tangent of a float (radians).",
                },
                BuiltinOp {
                    name: "Float.log",
                    effect: "Float",
                    op: "log",
                    signature: "log(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the natural logarithm. Returns nil for x ≤ 0.",
                },
                BuiltinOp {
                    name: "Float.exp",
                    effect: "Float",
                    op: "exp",
                    signature: "exp(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute e to the power of x.",
                },
                BuiltinOp {
                    name: "Float.log2",
                    effect: "Float",
                    op: "log2",
                    signature: "log2(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the base-2 logarithm. Returns nil for x ≤ 0.",
                },
                BuiltinOp {
                    name: "Float.log10",
                    effect: "Float",
                    op: "log10",
                    signature: "log10(x: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Compute the base-10 logarithm. Returns nil for x ≤ 0.",
                },
                BuiltinOp {
                    name: "Float.pow",
                    effect: "Float",
                    op: "pow",
                    signature: "pow(base: Float, exp: Float) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Raise base to the exp power (base^exp).",
                },
                BuiltinOp {
                    name: "String.to_int",
                    effect: "String",
                    op: "to_int",
                    signature: "to_int(value: String) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Parse a string to an integer. Returns 0 for invalid input.",
                },
                BuiltinOp {
                    name: "String.to_float",
                    effect: "String",
                    op: "to_float",
                    signature: "to_float(value: String) -> Float",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Parse a string to a float. Returns 0.0 for invalid input.",
                },

                BuiltinOp {
                    name: "String.length",
                    effect: "String",
                    op: "length",
                    signature: "length(s: String) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the length of the string in bytes.",
                },
                BuiltinOp {
                    name: "String.charAt",
                    effect: "String",
                    op: "charAt",
                    signature: "charAt(s: String, index: Int) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the byte at the given index in the string, or -1 if out of bounds.",
                },
                BuiltinOp {
                    name: "String.from_char",
                    effect: "String",
                    op: "from_char",
                    signature: "from_char(code: Int) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Create a single-character string from a Unicode code point. Returns nil for invalid code points (surrogates, out of range).",
                },
                BuiltinOp { name: "String.concat", effect: "String", op: "concat", signature: "concat(a: String, b: String) -> String", implemented_in: ImplSite::StandaloneVm, description: "Concatenate two strings.", },
                BuiltinOp { name: "String.substring", effect: "String", op: "substring", signature: "substring(s: String, start: Int, len: Int) -> String", implemented_in: ImplSite::StandaloneVm, description: "Extract a substring.", },
                BuiltinOp {
                    name: "Time.now",
                    effect: "Time",
                    op: "now",
                    signature: "now() -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the current Unix timestamp (seconds since 1970-01-01 00:00:00 UTC).",
                },
                BuiltinOp {
                    name: "Timer.sleep",
                    effect: "Timer",
                    op: "sleep",
                    signature: "sleep(name: String, duration_ms: Int) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Schedule a durable workflow timer; only available inside workflow actors.",
                },
                BuiltinOp {
                    name: "Signal.wait",
                    effect: "Signal",
                    op: "wait",
                    signature: "wait(name: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Suspend the workflow until the named signal arrives, then resume with unit.",
                },
                BuiltinOp {
                    name: "Inference.ask",
                    effect: "Inference",
                    op: "ask",
                    signature: "ask(prompt: String) -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Send the prompt to the configured inference provider and return the reply; suspends non-blockingly when the runtime supports it.",
                },
                BuiltinOp {
                    name: "Http.get",
                    effect: "Http",
                    op: "get",
                    signature: "get(url: String) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Perform an HTTP GET request to `url` and return the response body as a string on success, nil on error. Requires the `http-client` or `ai-runtime` feature for the reqwest provider.",
                },
                BuiltinOp {
                    name: "Http.post",
                    effect: "Http",
                    op: "post",
                    signature: "post(url: String, body: String) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Perform an HTTP POST request to `url` with a JSON body and return the response body as a string on success, nil on error. Requires the `http-client` or `ai-runtime` feature for the reqwest provider.",
                },
                BuiltinOp {
                    name: "Http.serve",
                    effect: "Http",
                    op: "serve",
                    signature: "serve(port: Int, handler: fn(String) -> String) -> Int",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Start an HTTP/1.1 server on `port`. For each request, calls `handler(body)` and returns the result as the response body with status 200. Returns the actual bound port.",
                },
                BuiltinOp {
                    name: "Web.route",
                    effect: "Web",
                    op: "route",
                    signature: "route(method: String, path: String, handler: fn() -> Html) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Register a request handler for the given method and path.",
                },
                BuiltinOp {
                    name: "Web.html",
                    effect: "Web",
                    op: "html",
                    signature: "html(tag: String, attrs: [(String, Html)], children: [Html]) -> Html",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Construct an HTML element from a tag, attributes, and children.",
                },
                BuiltinOp {
                    name: "Web.text",
                    effect: "Web",
                    op: "text",
                    signature: "text(s: String) -> Html",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Escape a string and wrap it as an Html text node.",
                },
                BuiltinOp {
                    name: "Web.raw",
                    effect: "Web",
                    op: "raw",
                    signature: "raw(s: String) -> Html",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Wrap a raw string as an Html value without escaping.",
                },
                BuiltinOp {
                    name: "Web.redirect",
                    effect: "Web",
                    op: "redirect",
                    signature: "redirect(url: String) -> Html",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Produce a redirect response to the given URL.",
                },
                BuiltinOp {
                    name: "Web.serve_static",
                    effect: "Web",
                    op: "serve_static",
                    signature: "serve_static(path: String) -> Html",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Serve the contents of a static file as an HTML response.",
                },
                BuiltinOp {
                    name: "Web.read_body",
                    effect: "Web",
                    op: "read_body",
                    signature: "read_body() -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Read the body of the current HTTP request.",
                },
                BuiltinOp {
                    name: "Web.param",
                    effect: "Web",
                    op: "param",
                    signature: "param(name: String) -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Get a route parameter from the current HTTP request.",
                },
                BuiltinOp {
                    name: "Web.header",
                    effect: "Web",
                    op: "header",
                    signature: "header(name: String) -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Get a request header from the current HTTP request.",
                },
                BuiltinOp {
                    name: "Web.cookie",
                    effect: "Web",
                    op: "cookie",
                    signature: "cookie(name: String) -> String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Get a cookie value from the current HTTP request by name.",
                },
                BuiltinOp {
                    name: "Web.set_cookie",
                    effect: "Web",
                    op: "set_cookie",
                    signature: "set_cookie(name: String, value: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Add a Set-Cookie header to the current HTTP response.",
                },
                BuiltinOp {
                    name: "Web.clear_cookie",
                    effect: "Web",
                    op: "clear_cookie",
                    signature: "clear_cookie(name: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Add a Set-Cookie header that clears the named cookie.",
                },
                BuiltinOp {
                    name: "Realtime.broadcast",
                    effect: "Realtime",
                    op: "broadcast",
                    signature: "broadcast(room: String, message: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Broadcast a message to all subscribers of a realtime room.",
                },
                BuiltinOp {
                    name: "Actor.link",
                    effect: "Actor",
                    op: "link",
                    signature: "link(target: Actor) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Link the current actor to `target`; abnormal exits propagate to linked peers. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.unlink",
                    effect: "Actor",
                    op: "unlink",
                    signature: "unlink(target: Actor) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Remove the link between the current actor and `target`. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.monitor",
                    effect: "Actor",
                    op: "monitor",
                    signature: "monitor(target: Actor) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Monitor `target` from the current actor; a DOWN system message is delivered when it exits. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.demonitor",
                    effect: "Actor",
                    op: "demonitor",
                    signature: "demonitor(target: Actor) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Stop the current actor's monitor on `target`, so no DOWN message is delivered. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.trap_exit",
                    effect: "Actor",
                    op: "trap_exit",
                    signature: "trap_exit(flag: Bool) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Set the current actor's trap_exits flag; when true, linked-peer exit signals arrive as system messages instead of killing it. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.exit",
                    effect: "Actor",
                    op: "exit",
                    signature: "exit(reason: Int | String) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Self-exit the current actor; 0/\"normal\", 1/\"error\", 2/\"kill\" select the reason, anything else is custom. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.register",
                    effect: "Actor",
                    op: "register",
                    signature: "register(name: String) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Register the current actor under `name` in the local actor registry. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Actor.unregister",
                    effect: "Actor",
                    op: "unregister",
                    signature: "unregister(name: String) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Remove `name` from the local actor registry.",
                },
                BuiltinOp {
                    name: "Actor.whereis",
                    effect: "Actor",
                    op: "whereis",
                    signature: "whereis(name: String) -> Actor | Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Look up `name` in the local actor registry; returns the actor ref, or nil when the name is not registered.",
                },
                BuiltinOp {
                    name: "Actor.set_priority",
                    effect: "Actor",
                    op: "set_priority",
                    signature: "set_priority(level: Int) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Set the current actor's scheduling priority: 0=High, 1=Normal, 2=Low (any other value selects Normal). Ready High-priority actors are scheduled before Normal, Normal before Low; affects scheduling only, not message order. Nil no-op outside an actor.",
                },
                BuiltinOp {
                    name: "Otp.create_supervisor",
                    effect: "Otp",
                    op: "create_supervisor",
                    signature: "create_supervisor(name: String, strategy: Int) -> Int | Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Create an OTP supervisor actor and return its id; strategy is 0=one_for_one, 1=one_for_all, 2=rest_for_one, 3=simple_one_for_one (any other value yields nil). Nil no-op outside a runtime.",
                },
                BuiltinOp {
                    name: "Otp.supervise_child",
                    effect: "Otp",
                    op: "supervise_child",
                    signature: "supervise_child(sup: Int, child: Actor, policy: Int) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Place an existing actor under a supervisor; policy is 0=permanent, 1=temporary, 2=transient, 3=respawn_on_node_loss (RFC 0014, durable children only; any other value is a no-op). Unknown supervisor ids are nil no-ops.",
                },
                BuiltinOp {
                    name: "Otp.set_template",
                    effect: "Otp",
                    op: "set_template",
                    signature: "set_template(sup: Int, type_name: String) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Set the child template of a simple_one_for_one supervisor to the named actor type, resolved against the performing module's actor metadata. Unknown types or supervisor ids are nil no-ops.",
                },
                BuiltinOp {
                    name: "Otp.start_child",
                    effect: "Otp",
                    op: "start_child",
                    signature: "start_child(sup: Int) -> Actor | Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Spawn a fresh child from a simple_one_for_one supervisor's template and supervise it; returns the child actor ref, or nil when the supervisor is unknown, has no template, or is not simple_one_for_one.",
                },
                BuiltinOp {
                    name: "Otp.terminate_child",
                    effect: "Otp",
                    op: "terminate_child",
                    signature: "terminate_child(sup: Int, child: Actor) -> Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Remove a child from supervision WITHOUT restarting it and exit it cleanly (Normal). Unknown supervisors or children are nil no-ops.",
                },
                BuiltinOp {
                    name: "Otp.child_count",
                    effect: "Otp",
                    op: "child_count",
                    signature: "child_count(sup: Int) -> Int | Nil",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Return the number of currently supervised children, or nil for an unknown supervisor id.",
                },
                BuiltinOp {
                    name: "Crdt.increment",
                    effect: "Crdt",
                    op: "increment",
                    signature: "increment(field: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Increment the current actor's CRDT-backed counter field (`gcounter`/`pncounter`). The op-set guard rejects other CRDT types (nil); nil also outside an actor.",
                },
                BuiltinOp {
                    name: "Crdt.decrement",
                    effect: "Crdt",
                    op: "decrement",
                    signature: "decrement(field: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Decrement the current actor's `pncounter` field. Rejected (nil) on any other CRDT type; nil also outside an actor.",
                },
                BuiltinOp {
                    name: "Crdt.add",
                    effect: "Crdt",
                    op: "add",
                    signature: "add(field: String, item: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Add an element to the current actor's CRDT set field (`gset`/`orset`/`aworset`). Rejected (nil) on any other CRDT type; nil also outside an actor.",
                },
                BuiltinOp {
                    name: "Crdt.remove",
                    effect: "Crdt",
                    op: "remove",
                    signature: "remove(field: String, item: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Remove an element from the current actor's CRDT set field (`orset`/`aworset`). Rejected (nil) on any other CRDT type; nil also outside an actor.",
                },
                BuiltinOp {
                    name: "Crdt.set",
                    effect: "Crdt",
                    op: "set",
                    signature: "set(field: String, value: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Write a value to the current actor's CRDT register field (`lwwregister`/`mvregister`). Rejected (nil) on any other CRDT type; nil also outside an actor.",
                },
                BuiltinOp {
                    name: "Crdt.read",
                    effect: "Crdt",
                    op: "read",
                    signature: "read(field: String) -> Int | String",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Read the current actor's CRDT field materialized value: the count for counters, the element count for sets/RGA, and the stored string for `lwwregister` (nil on a missing field or outside an actor).",
                },
                BuiltinOp {
                    name: "Env.get",
                    effect: "Env",
                    op: "get",
                    signature: "get(name: String) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the value of the named environment variable, or nil if not set.",
                },
                BuiltinOp {
                    name: "Process.run",
                    effect: "Process",
                    op: "run",
                    signature: "run(cmd: String) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Execute a shell command via /bin/sh -c and return its stdout; returns nil on error or non-zero exit.",
                },
                BuiltinOp {
                    name: "System.arg",
                    effect: "System",
                    op: "arg",
                    signature: "arg(n: Int) -> String",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return the n-th command-line argument (0-based), or nil if out of range. Includes the program name at index 0.",
                },
                BuiltinOp {
                    name: "Python.import",
                    effect: "Python",
                    op: "import",
                    signature: "import(module: String) -> Unit",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Import a Python module.",
                },
                BuiltinOp {
                    name: "Python.call",
                    effect: "Python",
                    op: "call",
                    signature: "call(module: String, function: String, ...args) -> a",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Call a Python function with arguments.",
                },
                BuiltinOp {
                    name: "Python.get_attr",
                    effect: "Python",
                    op: "get_attr",
                    signature: "get_attr(module: String, attr: String) -> a",
                    implemented_in: ImplSite::RuntimeHost,
                    description: "Get an attribute from a Python module.",
                },
                BuiltinOp {
                    name: "Random.int",
                    effect: "Random",
                    op: "int",
                    signature: "int(lo: Int, hi: Int) -> Int",
                    implemented_in: ImplSite::StandaloneVm,
                    description: "Return a random integer in the inclusive range [lo, hi].",
                },
            ],
        }
    }

    /// All registered built-in operations, in registration order.
    pub fn ops(&self) -> &[BuiltinOp] {
        &self.ops
    }

    /// Look up a built-in by its fully-qualified name (e.g. `"IO.print"`).
    pub fn lookup(&self, name: &str) -> Option<&BuiltinOp> {
        self.ops.iter().find(|op| op.name == name)
    }

    /// Look up a built-in by fully-qualified name, or fail with a
    /// descriptive error naming the unknown operation.
    pub fn require(&self, name: &str) -> NuResult<&BuiltinOp> {
        self.lookup(name).ok_or_else(|| NuError::RuntimeError {
            msg: format!("unknown built-in operation '{}'", name),
            span: Span::default(),
        })
    }

    /// Distinct effect names covered by the registry, in first-seen order.
    pub fn effects(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for op in &self.ops {
            if !out.contains(&op.effect) {
                out.push(op.effect);
            }
        }
        out
    }
}

impl Default for StdLib {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// stdlib_docs: human-readable reference
// ---------------------------------------------------------------------------

/// Print a human-readable reference of every built-in effect operation
/// currently wired into the VM and runtime.
pub fn stdlib_docs() -> String {
    let lib = StdLib::new();
    let mut out = String::new();
    out.push_str("Nulang standard library — built-in effect operations\n");
    out.push_str("======================================================\n\n");
    for effect in lib.effects() {
        out.push_str(&format!("effect {}\n", effect));
        for op in lib.ops().iter().filter(|op| op.effect == effect) {
            let site = match op.implemented_in {
                ImplSite::StandaloneVm => "standalone VM",
                ImplSite::RuntimeHost => "runtime host",
            };
            out.push_str(&format!(
                "  {}  [{}]\n      {}\n",
                op.signature, site, op.description
            ));
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Behavior contracts
// ---------------------------------------------------------------------------

/// A behavior contract that an actor can declare it implements.
/// The compiler verifies that the actor has all required handler behaviors
/// with compatible signatures.
#[derive(Debug, Clone)]
pub struct BehaviorContract {
    /// Contract name (e.g. "StatefulService").
    pub name: &'static str,
    /// Required handler behaviors. Each entry is `(handler_name, param_count)`.
    /// The compiler checks that the actor declares a behavior with matching
    /// name and compatible parameter count.
    pub required_handlers: &'static [(&'static str, usize)],
    /// Human-readable description.
    pub description: &'static str,
}

/// Built-in behavior contracts that the compiler knows about.
pub const BUILTIN_CONTRACTS: &[BehaviorContract] = &[BehaviorContract {
    name: "StatefulService",
    required_handlers: &[
        ("init", 1),
        ("handle_call", 2),
        ("handle_cast", 1),
        ("handle_info", 1),
        ("terminate", 1),
    ],
    description:
        "Erlang/OTP gen_server-style stateful service with init/call/cast/info/terminate handlers.",
}];

/// Look up a built-in behavior contract by name.
pub fn lookup_contract(name: &str) -> Option<&'static BehaviorContract> {
    BUILTIN_CONTRACTS.iter().find(|c| c.name == name)
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_expected_builtins() {
        let lib = StdLib::new();
        for name in [
            "IO.print",
            "IO.println",
            "IO.read",
            "Timer.sleep",
            "FS.read",
            "FS.write",
            "FS.append",
            "Signal.wait",
            "Actor.link",
            "Actor.unlink",
            "Actor.monitor",
            "Actor.demonitor",
            "Actor.trap_exit",
            "Actor.exit",
            "Actor.register",
            "Actor.unregister",
            "Actor.whereis",
            "Actor.set_priority",
            "Otp.create_supervisor",
            "Otp.supervise_child",
            "Otp.set_template",
            "Otp.start_child",
            "Otp.terminate_child",
            "Otp.child_count",
            "Crdt.increment",
            "Crdt.decrement",
            "Crdt.add",
            "Crdt.remove",
            "Crdt.set",
            "Crdt.read",
        ] {
            assert!(
                lib.lookup(name).is_some(),
                "registry must contain built-in '{}'",
                name
            );
        }
    }

    #[test]
    fn registry_entries_are_consistent() {
        let lib = StdLib::new();
        for op in lib.ops() {
            assert_eq!(
                format!("{}.{}", op.effect, op.op),
                op.name,
                "name must equal effect.op for '{}'",
                op.name
            );
            assert!(!op.signature.is_empty(), "'{}' needs a signature", op.name);
            assert!(
                op.signature.starts_with(op.op),
                "signature of '{}' must start with the op name",
                op.name
            );
            assert!(
                !op.description.is_empty(),
                "'{}' needs a description",
                op.name
            );
        }
    }

    #[test]
    fn lookup_reports_impl_sites() {
        let lib = StdLib::new();
        assert_eq!(
            lib.lookup("IO.print").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("IO.read").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("FS.read").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("FS.write").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("FS.append").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("FS.exists").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("Timer.sleep").unwrap().implemented_in,
            ImplSite::RuntimeHost
        );
        assert_eq!(
            lib.lookup("Actor.link").unwrap().implemented_in,
            ImplSite::RuntimeHost
        );
        assert_eq!(
            lib.lookup("Actor.whereis").unwrap().implemented_in,
            ImplSite::RuntimeHost
        );
        assert_eq!(
            lib.lookup("Http.get").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
        assert_eq!(
            lib.lookup("Http.post").unwrap().implemented_in,
            ImplSite::StandaloneVm
        );
    }

    #[test]
    fn effects_lists_distinct_effects_in_order() {
        let lib = StdLib::new();
        assert_eq!(
            lib.effects(),
            vec![
                "IO",
                "Debug",
                "FS",
                "Array",
                "StrBuilder",
                "Map",
                "Test",
                "Int",
                "Float",
                "String",
                "Time",
                "Timer",
                "Signal",
                "Inference",
                "Http",
                "Web",
                "Realtime",
                "Actor",
                "Otp",
                "Crdt",
                "Env",
                "Process",
                "System",
                "Python",
                "Random",
            ]
        );
    }
    #[test]
    fn lookup_unknown_returns_none() {
        let lib = StdLib::new();
        assert!(lib.lookup("Net.send").is_none());
        assert!(lib.lookup("IO.nonexistent").is_none());
    }

    #[test]
    fn require_unknown_is_an_error() {
        let lib = StdLib::new();
        let err = lib.require("Net.send").unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("Net.send"),
            "error must name the operation: {}",
            msg
        );
    }

    #[test]
    fn docs_mention_every_registered_op() {
        let docs = stdlib_docs();
        let lib = StdLib::new();
        for op in lib.ops() {
            assert!(
                docs.contains(op.signature),
                "docs must include the signature of '{}'",
                op.name
            );
            assert!(
                docs.contains(op.description),
                "docs must include the description of '{}'",
                op.name
            );
        }
    }
}
