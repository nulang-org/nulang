# Getting Started with Nulang

> Verified against the example suite in `examples/` (commit `1fd62a5`).

## 1. What is Nulang?

Nulang is an actor-based programming language with algebraic effects and
capability tracking. Functions express side effects through `perform`/`handle`;
the type system tracks which effects a function uses. Actors provide concurrent
state isolation with message-passing — no locks, no shared mutable state.

## 2. Install & Run

Build from source:

```bash
git clone https://github.com/nulang-org/nulang.git && cd nulang
cargo build --release
```

Run a file:

```bash
nulang hello.nula        # compile + run
nulang --check hello.nula # type-check only, don't run
nulang --eval '40 + 2'    # evaluate inline code
nulang --repl             # start interactive REPL
```

In the REPL, try `:help` for commands, `:type <expr>` to inspect a type,
`:load <file>` to run a file, and use Tab for completion.

## 3. Hello World

`perform IO.print` writes to stdout. Use `+` for string concatenation and
`Int.to_string` to convert numbers.

```nula
perform IO.print("Hello, Nulang!")

let name = "World"
perform IO.print("Hello, " + name + "!")

let answer = 42
perform IO.print("The answer is " + perform Int.to_string(answer))
```

> Run: `nulang examples/01_hello.nula`

## 4. Values & Bindings

### `let` (immutable) vs `var` (mutable)

`let` bindings cannot be reassigned. `var` bindings can:

```nula
let x = 42        // immutable — x = 99 would be an error
var y = 0         // mutable
y = y + 1         // ok
```

Top-level `var` is also valid:

```nula
var total = 0
total = total + 5
total = total * 2   // → 10
```

### Arithmetic

```nula
let a = 40 + 2       // 42
let b = 6 * 7        // 42
let c = 85 % 42      // 1
let d = 2 ** 10      // 1024  (exponentiation, right-associative)
let e = 2 * 3 ** 2   // 18    (** binds tighter than *)
```

Comparisons: `==`, `!=`, `<`, `>`. Booleans: `true`, `false`, `and`, `or`, `not`.

### Strings

Single-quoted strings and string concatenation:

```nula
let greeting = "Hello"
let target = "Nulang"
perform IO.print(greeting + ", " + target + "!")
```

Triple-quoted multi-line strings:

```nula
let poem = """Roses are red,
Violets are blue."""
```

Unicode escapes:

```nula
perform IO.print("\u{1F600}")       // 😀
perform String.length("\u{41}")     // 1  ('A')
```

### Type Annotations

```nula
let pi: Int = 3
```

## 5. Functions, Closures & Recursion

Functions are values. Use `fn(params) { body }` for closures. Multi-argument
functions separate parameters with commas:

```nula
let greet = fn(name) { "Hello, " + name + "!" }
let add = fn(x, y) { x + y }

let sum = add(40, 2)   // 42
```

Block expressions return the last value:

```nula
let result = {
    let a = 10
    let b = 20
    a + b             // → 30
}
```

Recursive closures use `let rec`:

```nula
let rec factorial = fn(n) {
    if n <= 1 then 1 else n * factorial(n - 1)
}
factorial(5)   // → 120

let rec fib = fn(n) {
    if n <= 1 then n else fib(n - 1) + fib(n - 2)
}
fib(10)        // → 55
```

> Full example: `examples/03_functions.nula`

## 6. Pattern Matching

Match on literals, variants, tuples, and records. Guards add conditions;
`@` creates alias patterns.

```nula
type Option[T] = Some(T) | None
type Color = Red | Green | Blue

let unwrap_or = fn(opt, default) {
    match opt with {
        | Some(x) => x
        | None => default
    }
}

// Guards
let classify = fn(n) {
    match n with {
        | x if x < 0 => "negative"
        | x if x == 0 => "zero"
        | x if x < 10 => "small"
        | _ => "large"
    }
}

// Record patterns
let area = fn(rect) {
    match rect with {
        | { w: width, h: height } => width * height
    }
}

// Alias: s @ Some(x) binds both the whole variant and the payload
let inspect = fn(opt) {
    match opt with {
        | s @ Some(x) => "Some(" + perform Int.to_string(x) + ")"
        | None => "None"
    }
}
```

> Full example: `examples/04_pattern_match.nula`

## 7. Records & Record Update

Records use `{ field: value }` syntax. Fields are accessed with `.` and can be
mutated:

```nula
let point = { x: 3, y: 4 }
point.x           // → 3

let counter = { value: 0 }
counter.value = counter.value + 10   // mutable field update

// Nested
let rect = { pos: { x: 10, y: 20 }, w: 30, h: 40 }
rect.pos.x = 100                     // nested mutation
```

**Record update** creates a new record from a base, overriding specific fields
(bases use `:`, overrides use `=`):

```nula
let p = { x: 1, y: 2 }
let q = { p .. y = 9 }    // q.x == 1, q.y == 9; p unchanged

// Multiple overrides
let r = { p .. x = 10, y = 20 }
```

> Full example: `examples/05_records.nula`

## 8. Higher-Order Functions & Pipe

Functions are first-class: pass them as arguments, return them from closures,
compose them:

```nula
let twice = fn(f, x) { f(f(x)) }
let inc = fn(n) { n + 1 }
twice(inc, 5)           // → 7

// Closure factory
let make_adder = fn(n) { fn(x) { x + n } }
let add5 = make_adder(5)
add5(10)                // → 15
```

The pipe operator `|>` chains transformations left-to-right:

```nula
let double = fn(n) { n * 2 }
let add_ten = fn(n) { n + 10 }

let result = 5
    |> double          // 10
    |> add_ten         // 20
    |> double          // 40

// Inline closures work too
let v = 41 |> fn(n) { n + 1 }   // → 42
```

> Full examples: `examples/06_higher_order.nula`, `examples/10_pipe.nula`

## 9. Effects

Nulang models side effects as algebraic effects. Use `perform` to request an
effect and `handle` to intercept it:

```nula
// Handle a custom effect
let answer = handle perform Math.getAnswer() with {
    | Math.getAnswer() => 42
}

// Multiple performs, single handler
let result = handle {
    let a = perform Math.getAnswer()
    let b = perform Math.getAnswer()
    a + b
} with {
    | Math.getAnswer() => 10
}

// Built-in effects (IO.print works without explicit handler)
perform IO.print("Hello from effects!")

perform String.length("Nulang")     // → 6
perform Int.to_string(2024)         // → "2024"
```

### FS Effect (file I/O)

File operations use the `FS` effect:

```nula
let content = perform FS.read("input.txt")    // returns String or nil
perform FS.write("output.txt", "Hello!")       // returns Unit
let exists = perform FS.exists("data.json")    // returns Bool
```

Operations: `perform FS.read(path)`, `perform FS.write(path, content)`,
`perform FS.append(path, content)`, `perform FS.exists(path)`.

> Full example: `examples/07_effects.nula`; see also `src/stdlib/fs.nula`

## 10. Actors

Actors encapsulate state and communicate via message passing. Declare an actor
with `state` fields and `behavior` handlers, then `spawn` and send messages with
`!`:

```nula
actor Counter {
    state count: Int = 0

    behavior increment(by: Int) {
        self.count = self.count + by
    }

    behavior show() {
        perform IO.print("Counter: " + perform Int.to_string(self.count))
    }
}

actor Greeter {
    state greeting: String = "Hello"
    state name: String = "World"

    behavior set_greeting(g: String) {
        self.greeting = g
    }

    behavior set_name(n: String) {
        self.name = n
    }

    behavior greet() {
        perform IO.print(self.greeting + ", " + self.name + "!")
    }
}

fn main() {
    let counter = spawn Counter {}
    counter ! increment(5)
    counter ! increment(3)
    counter ! show()                         // → "Counter: 8"

    let greeter = spawn Greeter {}
    greeter ! set_name("Actor System")
    greeter ! greet()                        // → "Hello, Actor System!"

    0
}
```

Spawn syntax: `spawn Actor { field = value }` (overrides state defaults).

> Full example: `examples/08_actors.nula`

## 11. Error Handling

Use `catch` to provide a fallback when a variant-producing computation returns
an error variant, and `fail` to exit early from a function:

```nula
type Result[Ok, Err] = Ok(Ok) | Error(Err)

fn ok_val() -> Result[Int, String] { Ok(42) }
fn err_val() -> Result[Int, String] { Error("fail") }

fn early_return(x: Int) -> Int {
    if x < 0 then fail 0 else x
}

fn div(a: Int, b: Int) -> Int ! String {
    if b == 0 then fail Error("div by zero") else Ok(a / b)
}

// catch: fallback for error variants
ok_val() catch 0    // → 42    (success — returns the Ok payload)
err_val() catch 0   // → 0     (error — returns the fallback)

// fail: early exit from a function
early_return(42)    // → 42
early_return(-5)    // → 0

// ?: unwrap the Ok variant
div(10, 2)?         // → 5
```

## 12. The Package Manager

Nulang ships with `nula`, a built-in package manager. All commands are invoked
as `nulang nula <cmd>`:

```bash
nulang nula new my-project     # scaffold a new package
nulang nula init               # scaffold in the current directory
nulang nula build              # resolve deps and type-check
nulang nula build-wasm         # build to .wasm (requires wasmtime)
nulang nula run                # build and run
nulang nula run --watch        # re-run on file changes
nulang nula test               # run test files in tests/
nulang nula test --filter foo  # run only tests matching "foo"
nulang nula add <name>         # add a dependency
nulang nula remove <name>      # remove a dependency
nulang nula list               # list resolved dependencies
nulang nula clean              # remove build artifacts
nulang nula doc                # generate API docs (docs/api.md)
nulang nula doc --open         # generate and open docs
```

`nula new` creates:

```
my-project/
  Nulang.toml          # [package] name = "my-project" …
  src/main.nula        # entry point
```

Dependencies are stored in `Nulang.toml` under `[dependencies]` and resolved to
`Nulang.lock`.

## 13. Testing

Test files live under `tests/` with a `.nula` extension. Use the built-in `Test`
effect for assertions:

```nula
// tests/math_tests.nula

fn test_addition() {
    perform Test.assert_eq(40 + 2, 42)
}

fn test_multiplication() {
    perform Test.assert(6 * 7 == 42, "multiplication failed")
}

fn test_truth() {
    perform Test.assert_true(true)
}

test_addition()
test_multiplication()
test_truth()
```

Run with:

```bash
nulang nula test
nulang nula test --filter addition
```

Available assertions: `perform Test.assert(cond, msg)`, `perform Test.assert_eq(actual, expected)`,
`perform Test.assert_true(cond)`, `perform Test.fail_with(msg)`.

## 14. Where to Go Next

- **`examples/`** — 17 verified, runnable examples from hello world through
  HTTP/JSON and actor-based URL fetching (see `examples/README.md`).
- **`SPEC2.md`** — language specification with formal syntax and semantics.
- **`CHANGELOG.md`** — per-commit feature log.
- **`src/stdlib/`** — standard library source (`fs.nula`, `test.nula`, …).
- **REPL** — launch with `nulang --repl`; use `:help`, `:type`, `:load`.

---

Comments use `//`. Em dashes (`—`) are not accepted by the lexer.
The entry point for a package is `src/main.nula` (the `fn main()` return value
is ignored).
