# Getting Started

Welcome to Nulang — a distributed, actor-based programming language that fuses
Erlang-style fault tolerance with a modern type system. This chapter gets you
from zero to a running Nulang program in about five minutes.

## Installation

Nulang is written in Rust. The fastest way to get started is to build from
source:

```bash
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --release
```

The binary lands at `target/release/nulang`. Add it to your `PATH` or symlink
it somewhere convenient:

```bash
ln -s "$(pwd)/target/release/nulang" ~/.local/bin/nulang
```

Prebuilt binaries are available on the [GitHub Releases
page](https://github.com/nulang-org/nulang/releases) for Linux (x86_64, aarch64)
and macOS (x86_64, aarch64).

Verify your installation:

```bash
nulang --version
```

## Your First Program

Nulang source files use the `.nula` extension. Create `hello.nula`:

```nula
perform IO.print("Hello, Nulang!")
```

Run it:

```bash
nulang hello.nula
```

The REPL is also available for interactive exploration:

```bash
nulang --repl
```

Try typing `1 + 2` and pressing Enter — the result prints immediately.

## Basic Syntax

### Values and Bindings

Nulang has the types you'd expect: `Int`, `Float`, `String`, `Bool`, `Unit`,
and `Nil`. Bind variables with `let`:

```nula
let x = 42
let name = "Nulang"
let active = true
let nothing = nil
let result = x + 8  -- 50
```

The `let` binding can scope a block with `in`:

```nula
let a = 10 in
let b = 20 in
a + b  -- 30
```

### Functions

Functions are first-class values. Define them with `fn`:

```nula
let greet = fn(name) {
    "Hello, " + name
}

greet("World")  -- "Hello, World"
```

Top-level functions use the `fn` declaration form, which supports optional type
annotations:

```nula
fn add(a: Int, b: Int) -> Int {
    a + b
}
```

Type annotations are optional — Nulang infers types with Hindley-Milner
inference.

### Control Flow

`if`/`then`/`else` is an expression, not a statement:

```nula
let status = if score > 50 then "pass" else "fail"
```

Pattern matching with `match` handles algebraic data types:

```nula
type Option[T] = Some(T) | None

fn unwrap_or(opt: Option[Int], default: Int) -> Int {
    match opt {
        case Some(x) => x,
        case None => default
    }
}
```

### Records and Tuples

```nula
let point = { x: 3, y: 4 }
point.x + point.y  -- 7

let pair = (1, "hello")
```

### The Pipe Operator

Use `|>` to chain function calls left to right:

```nula
let result = 5
    |> fn(n) { n + 3 }    -- 8
    |> fn(n) { n * 2 }    -- 16
    |> fn(n) { n - 1 }    -- 15
```

## Actors

Actors are the central abstraction in Nulang. An actor has private state and
responds to messages through behaviors — functions that receive a message and
can read or update the actor's state. Actors are isolated: no two actors share
memory, and all communication is through asynchronous message passing.

### Declaring and Spawning

Define an actor with the `actor` keyword, then spawn an instance:

```nula
actor Counter {
    state count = 0

    behavior inc() {
        self.count = self.count + 1
    }

    behavior get() {
        self.count
    }
}

let counter = spawn Counter {}
```

### Sending Messages

Use the send operator `!` or the `send` keyword:

```nula
counter ! inc()
counter ! inc()
counter ! inc()
```

Actors process messages one at a time in FIFO order. Each behavior runs to
completion before the next message is processed — there are no data races on
actor state.

### Request-Response with `ask`

`ask` sends a message and blocks until the response arrives:

```nula
actor Adder {
    state total = 0
    behavior add(x: Int) { self.total = self.total + x }
    behavior sum() { self.total }
}

let adder = spawn Adder {}
adder ! add(10)
adder ! add(20)
ask adder sum()   -- 30
```

## Algebraic Effects

Effects let you separate *what* a computation does from *how* it does it. A
computation performs an effect; a handler decides what that effect means.

Effects are resolved by name at runtime — no declaration is needed. Perform an
effect with `perform`, and intercept it with `handle`:

```nula
handle perform Math.getAnswer() with {
    Math.getAnswer() => 42
}
```

The `handle` expression catches any `perform Math.getAnswer()` inside its body
and routes it to the handler arm, which provides a value. The computation
resumes after the handler runs — effects are not exceptions.

### Built-in Effects

Nulang ships with several built-in effects:

| Effect | Operation | Description |
|--------|-----------|-------------|
| `IO` | `IO.print(msg)` | Print to stdout |
| `IO` | `IO.read()` | Read a line from stdin |
| `Int` | `Int.to_string(n)` | Convert int to string |
| `Timer` | `Timer.sleep("name", ms)` | Suspend for `ms` milliseconds |
| `Signal` | `Signal.wait("name")` | Wait for an external signal |

## A Complete Example: Chat Room

Let's build a simple chat room: actors broadcast messages through a shared
room actor.

```nula
actor ChatRoom {
    state messages: Int = 0

    behavior broadcast(sender_name: String, text: String) {
        self.messages = self.messages + 1
        perform IO.print("[" + sender_name + "]: " + text)
    }

    behavior count() {
        perform IO.print(
            "Total messages: " + perform Int.to_string(self.messages)
        )
    }
}

actor ChatClient {
    state room_ref = 0

    behavior init(room: Int) {
        self.room_ref = room
    }

    behavior say(name: String, text: String) {
        self.room_ref ! broadcast(name, text)
    }
}

fn main() {
    let room = spawn ChatRoom {}

    let alice = spawn ChatClient {}
    let bob = spawn ChatClient {}

    alice ! init(room)
    bob ! init(room)

    alice ! say("Alice", "Hello!")
    bob ! say("Bob", "Hi Alice!")

    room ! count()
    0
}
```

## Next Steps

- **Examples**: Browse `examples/` for larger programs — AI chat, worker pools,
  supervisor trees, distributed counters.
- **Language Reference**: `SPEC2.md` is the full language specification.
- **API Documentation**: Run `nulang --doc` to generate `docs/api.md` from doc
  comments in the standard library.
- **RFCs**: The `RFC/` directory contains proposals for format stability,
  deprecation cycles, and roadmap items.
