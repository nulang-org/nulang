---
title: Standard Library Overview
description: Built-in effects and operations wired into the Nulang VM and runtime.
---

## Built-in Effects

Nulang ships with a set of built-in effects wired directly into the VM and runtime. These effects provide core functionality — I/O, collections, strings, timing, HTTP, web serving, actor management, OTP supervision, and AI inference.

Each effect groups related operations accessed via `perform Effect.operation(...)`:

| Effect | Operations | Purpose |
|--------|-----------|---------|
| [IO](/stdlib/io/) | `print`, `println`, `read`, `log`, `log_error` | Console input/output |
| [Int](/stdlib/int/) | `to_string`, `to_float`, `to_hex`, `to_binary` | Integer conversions |
| [Float](/stdlib/float/) | `to_int`, `to_string`, `sin`, `cos`, `sqrt`, `tan`, `log`, `exp`, `log2`, `log10`, `pow` | Float math and conversions |
| [String](/stdlib/string/) | `to_int`, `to_float`, `length`, `charAt`, `from_char`, `concat`, `substring` | String operations |
| [Array](/stdlib/array/) | `length`, `push`, `new`, `set`, `slice` | Immutable array operations (value semantics) |
| [Map](/stdlib/map/) | `new`, `insert`, `get`, `contains`, `remove`, `size` | Immutable map operations |
| [StrBuilder](/stdlib/strbuilder/) | `new`, `push`, `to_string`, `len`, `reset` | Efficient string building |
| [Debug](/stdlib/debug/) | `inspect` | Labeled value inspection |
| [Test](/stdlib/test/) | `assert`, `assert_eq`, `assert_true` | Test assertions |
| [Timer](/stdlib/timer/) | `sleep` | Durable workflow timers |
| [Time](/stdlib/time/) | `now` | Wall-clock time |
| [Signal](/stdlib/signal/) | `wait` | Workflow signal suspension |
| [Random](/stdlib/random/) | `int` | Random integers |
| [FS](/stdlib/fs/) | `read`, `write`, `append`, `exists` | File-system access |
| [Env](/stdlib/env/) | `get` | Environment variables |
| [System](/stdlib/system/) | `arg` | Command-line arguments |
| [Process](/stdlib/process/) | `run` | External process execution |
| [Python](/stdlib/python/) | `import`, `call`, `get_attr` | Python interop |
| [Http](/stdlib/http/) | `get`, `post`, `serve` | HTTP client and server |
| [Web](/stdlib/web/) | `route`, `html`, `text`, `raw`, `redirect`, `serve_static`, `read_body`, `param`, `header`, `cookie`, `set_cookie`, `clear_cookie` | Web framework |
| [Realtime](/stdlib/realtime/) | `broadcast` | Real-time broadcast to connected clients |
| [Inference](/stdlib/inference/) | `ask` | AI inference queries (canonical name; `LLM.ask` remains as a deprecated alias) |
| [Actor](/stdlib/actor/) | `link`, `unlink`, `monitor`, `demonitor`, `trap_exit`, `exit`, `register`, `unregister`, `whereis`, `set_priority` | Actor lifecycle management |
| [Otp](/stdlib/otp/) | `create_supervisor`, `supervise_child`, `set_template`, `start_child`, `terminate_child`, `child_count` | OTP supervision trees |
| [Crdt](/stdlib/crdt/) | `increment`, `decrement`, `add`, `remove`, `set`, `read` | Replicated CRDT state |

## Implementation Sites

Built-in operations are implemented in one of two places:

- **Standalone VM** (`ImplSite::StandaloneVm`) — Operations handled by the VM directly, available in actor-free scripts (e.g., REPL, one-shot `--eval`).
- **Runtime Host** (`ImplSite::RuntimeHost`) — Operations that require the actor runtime, reached through the `ActorVmCallbacks` trait. These are nil no-ops outside an actor context.

## Using Built-in Effects

```nulang
// IO effects work everywhere
perform IO.print("Hello, World!")
perform IO.println("With a newline")
let input = perform IO.read()

// Actor effects require the runtime
perform Actor.register("my_service")
perform Actor.link(some_actor)

// OTP effects for supervision
let sup = perform Otp.create_supervisor("my_sup", 0)
```

## Adding New Built-in Effects

New built-in operations are registered in the `StdLib` registry in `src/stdlib.rs` in the Nulang repository; see the contributor documentation for the full walkthrough. The per-effect reference pages under this section are auto-generated from that registry.
