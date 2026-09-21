# Common Pitfalls & Idioms

Nulang may accept compatibility spellings while syntax evolves, but
`nulang fmt` defines the canonical source form. Each section below is a
one-line rule, a ❌ wrong snippet, and a ✅ canonical snippet — all verified
against `examples/`, `docs/GETTING_STARTED.md`, and the integration-test
suite.

> **Verified against commit `e0cf432`.**  Several previously-listed
> pitfalls have been fixed — see *Recently Fixed* at the bottom.

---

### 1. Imports use `::`, not `.`

Module paths use `::` as separator.  Stdlib modules live under
`stdlib::*` and are resolved via `NULANG_STDLIB` (or `src/stdlib/` in
development).

```nula
// ❌
import stdlib.list

// ✅
import stdlib::list
```

Available stdlib modules that parse and import correctly:
`math`, `list`, `string`, `set`, `map`, `core`, `test`, `fs`,
`option`, `result`, `datetime`, `json`, `http`

All 13 modules have working VM primitives and are fully functional.
The `json` module now includes a working `parse` + `stringify` pair.

---

### 2. Imported names are unqualified

`import stdlib::test` brings every `pub` declaration into scope
*without* a module prefix.  Call them directly — no `test.` qualifier.

```nula
import stdlib::test

// ❌
test.assert_eq(40 + 2, 42)

// ✅
assert_eq(40 + 2, 42)
```

> **Built-in effects don't need an import at all.**  You can write
> `perform Test.assert_eq(a, b)` or `perform FS.read(path)` directly,
> without any `import`.  The `Test`, `FS`, `IO`, `String`, and `Int`
> effects are wired into the VM.

---

### 3. Built-in effects are called with `perform`

Every built-in effect operation requires the `perform` keyword.  These
are VM-wired — they work without an `import` statement.

```nula
// ❌
IO.print("hello")
FS.read("data.txt")
Test.assert_eq(40 + 2, 42)

// ✅
perform IO.print("hello")
perform FS.read("data.txt")
perform Test.assert_eq(40 + 2, 42)
perform Int.to_string(42)
perform String.length("hello")
```

---

### 4. `let` is immutable; `var` is mutable

`let` bindings cannot be reassigned.  Use `var` when you need mutation.

```nula
// ❌
let x = 0
x = 1                // error: x is immutable

// ✅
let x = 0            // immutable — fine if never reassigned
var x = 0            // mutable
x = x + 1            // ok
```

---

### 5. Comments are `//`, not `--`

Em dashes (`—`) are rejected by the lexer.  Only `//` to end-of-line
and `/* ... */` block comments are accepted.

```nula
// ❌
-- this is not a comment
// ✅
// this is a comment
```

---

### 6. Record literals use `:` — record updates use `=`

Field definitions in a literal are `field: value`.  When updating an
existing record (`{ base .. overrides }`), overrides use `=` — the colons
come from the *base* record.

```nula
// ❌
let p = { x = 1, y = 2 }          // literals need `:`
let q = { p .. y: 9 }            // overrides need `=`

// ✅
let p = { x: 1, y: 2 }           // literal
let q = { p .. y = 9 }           // override y, keep x from p
let r = { p .. x = 10, y = 20 }  // multiple overrides
```

---

### 7. `spawn` named fields use `=`

Canonical actor construction is call-like. State-field overrides use
`FieldName = value` (not `:`) and override defaults declared by the actor.

```nula
actor Counter {
    state count: Int = 0
    behavior add(n: Int) { self.count = self.count + n }
}

// ❌
let c = spawn Counter(count: 42)

// ✅
let c = spawn Counter(count = 42)
```

The parser still accepts the older brace form during migration, but
`nulang fmt` emits `spawn Counter(...)`.

---

### 8. Local message sends canonically use `!`, not `.`

Sending a local asynchronous message canonically uses the `!` operator.
The dotted form (`actor.field`) is field *access*, not a send. The parser
also accepts `send actor behavior(...)` for compatibility; the formatter
rewrites local sends to `!`. Explicit `send remote ...` remains keyword
syntax because the transport choice changes semantics.

```nula
// ❌
counter.increment(5)       // field access, not a message send

// ✅
counter ! increment(5)     // send the `increment` message
```

---

### 9. `let … in` scopes to the body; a block-`let` scopes to the rest of the block

**Expression form:** `let x = V in BODY` — `x` is visible *only* in
`BODY`, not after the `in`.

**Statement form:** `let x = V` (without `in`, at block/statement level) —
`x` is visible for the remainder of the enclosing block.

The gotcha: the `in` form shadows tightly — an inner `let … in` does not
affect code that follows in the same block.

```nula
let x = 1 in {
    let x = 10 in 0    // inner x shadows outer — but only in `0`
    x                   // → 1 (the outer x! inner x's scope already closed)
}

// If you want the inner x to reach past the `in`, use a block-`let`:
let x = 1 in {
    let x = 10          // statement-form: scopes to end of block
    x                   // → 10
}
```

---

### 10. `if` uses `then` / `else`

Conditions don't need parentheses, and the branches are separated by
`then` and `else`.

```nula
// ❌
if n <= 1 { 1 } else { n * factorial(n - 1) }

// ✅
if n <= 1 then 1 else n * factorial(n - 1)
```

---

### 11. `match` arms are `|`-prefixed; guards use `if`

Every arm starts with `|`.  Guards attach to a bound variable —
a guard variable must be *bound* by its pattern.

```nula
// ❌
match n with {
    x if x < 0 => "negative"       // missing leading `|`
    _ if x < 0 => "negative"       // `_` does NOT bind `x`
}

// ✅
match n with {
    | x if x < 0  => "negative"
    | x if x == 0 => "zero"
    | _           => "non-negative"
}
```

---

### 12. `**` is right-associative and binds tighter than `*`

Exponentiation groups right-to-left and has higher precedence than
multiplication.

```nula
2 ** 3 ** 2    // → 2 ** (3 ** 2) = 2 ** 9 = 512  (right-associative)
2 * 3 ** 2     // → 2 * (3 ** 2)  = 2 * 9  = 18   (tighter than *)
```

---

### 13. Multi-line strings use `"""`; unicode via `\u{…}`

Triple-quoted strings span multiple lines.  Unicode escapes use the
`\u{hex}` form (not `\uXXXX`).

```nula
// ❌
let s = "line1\nline2"
let smile = "\u1F600"

// ✅
let s = """line1
line2"""
let smile = "\u{1F600}"          // 😀
```

---

### 14. `catch` (postfix or prefix) and `fail`

Both forms of `catch` desugar to a `match` on `Ok`/`Error` variants.
`fail` produces an early-exit `Error` value.

```nula
type Result[Ok, Err] = Ok(Ok) | Error(Err)

// Postfix catch
ok_val() catch 0                  // Ok(42)  → 42
err_val() catch 0                 // Error(_) → 0

// Prefix catch (same semantics)
catch ok_val() 0                  // → 42
catch err_val() 0                 // → 0

// fail: early exit from a function
fn div(a: Int, b: Int) -> Int ! String {
    if b == 0 then fail Error("div by zero") else Ok(a / b)
}
```

> `?` unwraps: `div(10, 2)?` → `5`.

---

### 15. `String.charAt` returns an Int code point, not a string

The VM primitive `String.charAt` returns the integer character code
(e.g. `104` for `'h'`), not a 1-character string.  The stdlib
`string` module provides a `char_at` wrapper that returns a string
via `String.substring`.

```nula
import stdlib::string

// ❌ — charAt returns an Int
let c = perform String.charAt("hello", 0)      // → 104

// ✅ — stdlib char_at returns a 1-char string
let ch = char_at("hello", 0)                   // → "h"

// ✅ — use substring directly
let s = perform String.substring("hello", 0, 1) // → "h"
```

> The stdlib `string` module also exports `char_code_at` if you
> *want* the integer code point.  See `src/stdlib/string.nula`.

---

### 16. Tuple field access `.0` / `.1`

Tuple fields are accessed with numeric indices: `t.0`, `t.1`, etc.
Chained access (`t.0.1`) works directly — no parens required
(fixed in commit `d91dcc6`).  If you hit an edge case, wrap with
parens: `(t.0).1`.

```nula
let t = (10, 20)
t.0                  // → 10
t.1                  // → 20

// Chained access on nested tuples
let p = ((1, 2), 3)
p.0.0                // → 1
p.0.1                // → 2
p.1                  // → 3

// When calling a function that returns a tuple:
let f = fn(x) (x, x + 1)
f(10).1              // → 11
```

> Tuple destructuring via `match` still works and is the idiomatic way
> to extract all fields at once: `match t with | (x, y) => x + y`.


---

### 17. List functions: some are Int-only, some are polymorphic

Stdlib `list` functions fall into two categories:

- **Polymorphic** (no `[Int]` annotation): `map`, `filter`, `reverse`,
  `take`, `drop`, `range` — work with any element type (strings,
  records, tuples, etc.).

- **Int-only** (annotated `[Int]`): `length`, `sum`, `contains`,
  `index_of`, `find`, `any`, `all`, `fold`, `max_of`, `min_of`,
  `sort`, `append`, `zip`, `enumerate` — produce a type error when
  passed non-Int arrays.

```nula
import stdlib::list

// ❌ length expects [Int]
let n = length(["x", "y", "z"])
// Error: Type mismatch: expected Int, found String

// ❌ fold expects init: Int, xs: [Int]
let r = fold(fn(acc, s) { acc + " " + s }, "hello", ["world"])
// Error: Type mismatch: expected Int, found String

// ✅ polymorphic functions work with any type
let excited = map(fn(s) { s + "!" }, ["a", "b", "c"])  // ["a!", "b!", "c!"]
let pos = filter(fn(r) { r.x > 0 }, [{x: 1}, {x: -1}, {x: 2}])  // [{x:1}, {x:2}]
```

---

## Quick Idioms
| Idiom | Snippet |
|-------|---------|
| Pipe (`|>`) | `41 |> fn(n) { n + 1 }` |
| Recursive closures | `let rec fib = fn(n) { … }` |
| Block expression | `{ let a = 10; let b = 20; a + b }` |
| Alias pattern | `| s @ Some(x) => …` |
| Type annotation | `let x: Int = 42` |
| Function type param | `fn map[T, U](arr: [T], f: fn(T) -> U) -> [U]` |
| Effect annotation | `fn div(a: Int, b: Int) -> Int ! String` |
| For loop | `for i in [1, 2, 3] { perform IO.print(i) }` |
| While loop | `while i < n { i = i + 1 }` |
| Array literal | `[1, 2, 3]` — `.len()`, `[i]`, `.push(v)` |
| Tuple | `(1, "hello")` — field access: `t.0`, `t.1`, `t.0.1` |
| String interpolation | `f"hello {name}"` |

---

## Recently Fixed

These used to be pitfalls but are now working correctly
(commits `e0cf432`, `fa98dcf`, `fd516ef`, `cb6ac4c`, `fcd4741`, `d91dcc6`, `8ca559c`, `3744b9d`, `6a55e0d`, `8f13bea`).

| Issue | Status | Notes |
|-------|--------|-------|
| Range expressions (`a .. b`) | ✅ Added | `for i in 0 .. 5 { … }`, bare `{ a .. b }` in blocks |
| LSP: code lenses, document links, hover docs | ✅ Added | Reference counts, clickable imports, enriched hover with doc comments |
| JSON parser (`parse` + `stringify`) | ✅ Added | Pure-Nulang recursive-descent; round-trips with full fidelity |
| Tuple `.0`/`.1` field access | ✅ Fixed | `t.0`, `t.0.1` chain directly |
| `a + b` with two `let`-bound strings | ✅ Fixed | Was returning `0`, now concatenates |
| All 13 stdlib modules import | ✅ Fixed | `option`, `result`, `datetime`, `json`, `http` now all working |
| `Array.push`/`Array.new`/`Array.length`/`Array.set`/`Array.slice` | ✅ Added | Full Array builtin with value semantics |
| `String.from_char` | ✅ Added | Code point → 1-char string; used by JSON parser |
| `var` bindings, record-update, ranges | ✅ Added | Mutable locals, `{ r .. f = v }` syntax, `a..b` expressions |
| `consume` / `recover` expressions | ✅ Added | `consume x` marks linear variable consumed; `recover { body }` is an isolated scope whose result must be sendable (it does not wrap the result in Ok/Error — see SPEC2 §3.9.2) |
| Numeric conversion primitives | ✅ Added | `Int.to_float`, `Float.to_int`, `Float.to_string`, `String.to_int`, `String.to_float` |
