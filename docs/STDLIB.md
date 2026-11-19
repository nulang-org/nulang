# Nulang Standard Library Reference

Covers the working stdlib modules as of commit `8ca559c`.

## Using the Standard Library

Import a module with `import stdlib::<module>` (double-colon path separator):

```nula
import stdlib::math
import stdlib::list
import stdlib::test
import stdlib::fs
import stdlib::core
import stdlib::set
import stdlib::map
import stdlib::string
```

Functions are called **unqualified** — `abs(-5)`, not `math.abs`. With imports from
multiple modules, avoid name collisions (e.g. `list::max_of` vs `math::max`,
or `set` and `map` both exporting `empty`/`size`/`remove`/`is_empty` — import
only one of the two per file).

### Locating the stdlib

From outside the project root, set the environment variable pointing at the
stdlib tree:

```sh
export NULANG_STDLIB=/path/to/nulang/src/stdlib
```

### Built-in Effects

Built-in effects (`FS`, `Test`, `IO`, `Array`, `String`, `Int`) are invoked with
`perform` and need **no** import:

```nula
perform Array.push(arr, elem)
perform FS.read("data.txt")
perform Test.assert(cond, msg)
```

---

## Module: math

Integer and floating-point numeric functions. No effect constraints — all
functions are pure.

| Function | Signature | Description |
|---|---|---|
| `abs` | `fn abs(x: Int) -> Int` | Absolute value. |
| `min` | `fn min(a: Int, b: Int) -> Int` | Minimum of two integers. |
| `max` | `fn max(a: Int, b: Int) -> Int` | Maximum of two integers. |
| `clamp` | `fn clamp(x: Int, lo: Int, hi: Int) -> Int` | Clamp `x` to the range `[lo, hi]`. |
| `sign` | `fn sign(x: Int) -> Int` | Sign: `-1` for negative, `0` for zero, `1` for positive. |
| `pow` | `fn pow(base: Int, exp: Int) -> Int` | Integer exponentiation (base^exp) for non-negative exp. |
| `factorial` | `fn factorial(n: Int) -> Int` | Factorial. Returns 1 for `n <= 1`. |
| `gcd` | `fn gcd(a: Int, b: Int) -> Int` | Greatest common divisor (Euclidean algorithm). |
| `lcm` | `fn lcm(a: Int, b: Int) -> Int` | Least common multiple. |
| `is_even` | `fn is_even(n: Int) -> Bool` | True when `n` is even. |
| `is_odd` | `fn is_odd(n: Int) -> Bool` | True when `n` is odd. |
| `sqrt` | `fn sqrt(x: Float) -> Float` | Square root via Newton's method. Returns `-1.0` for negative input. |

### Usage

```nula
import stdlib::math

let a = abs(-5)          // 5
let m = clamp(12, 0, 10) // 10
let p = pow(2, 8)        // 256
let g = gcd(48, 18)      // 6
let r = sqrt(25.0)       // ~5.0
```

---

## Module: list

List/array operations over native arrays. Functions fall into two categories:

- **Int-only** (annotated `[Int]`): `sum`, `fold`, `max_of`, `min_of`,
  `sort`, `append`, `zip`, `enumerate`, and `uniq`.

- **Polymorphic** (no element-type annotation): `length`, `contains`, `index_of`,
  `find`, `any`, `all`, `map`, `filter`, `reverse`, `take`, `drop`,
  and `range`. These accept arrays of any compatible element type.

### Traversal functions

| Function | Signature | Description |
|---|---|---|
| `length` | `fn length(xs) -> Int` | Number of elements in the array. |
| `sum` | `fn sum(xs: [Int]) -> Int` | Sum of all elements. |
| `contains` | `fn contains(x, xs) -> Bool` | True if `x` appears in `xs`. |
| `index_of` | `fn index_of(x, xs) -> Option[Int]` | First index of `x`, or `None` if not found. |
| `find` | `fn find(pred, xs) -> Option[T]` | First matching element, or `None`. |
| `any` | `fn any(pred, xs) -> Bool` | True if any element satisfies the predicate. |
| `all` | `fn all(pred, xs) -> Bool` | True if all elements satisfy the predicate. |
| `fold` | `fn fold(f, init: Int, xs: [Int]) -> Int` | Left fold: `fold(f, init, [a,b,c])` = `f(f(f(init, a), b), c)`. |
| `max_of` | `fn max_of(xs: [Int]) -> Option[Int]` | Maximum element, or `None` for an empty array. |
| `min_of` | `fn min_of(xs: [Int]) -> Option[Int]` | Minimum element, or `None` for an empty array. |

### Array-producing functions

| Function | Signature | Description |
|---|---|---|
| `map` | `fn map(f, xs)` | Apply `f` to each element, return new array. |
| `filter` | `fn filter(pred, xs)` | Keep elements where `pred` returns true. |
| `reverse` | `fn reverse(xs)` | Return new array with elements in reverse order. |
| `take` | `fn take(n, xs)` | Take first `n` elements. |
| `drop` | `fn drop(n, xs)` | Drop first `n` elements. |
| `range` | `fn range(n)` | Array of `[0, 1, ..., n-1]`. |

### Usage

```nula
import stdlib::list

let xs = [1, 2, 3, 4, 5]

// Traversals
let s = sum(xs)                // 15
let c = contains(3, xs)        // true
let i = index_of(4, xs)        // Some(3)
let y = any(fn(x) { x > 3 }, xs)   // true
let z = fold(fn(a, b) { a + b }, 0, xs)  // 15

// Array-producing
let doubled = map(fn(x) { x * 2 }, xs)  // [2, 4, 6, 8, 10]
let evens   = filter(fn(x) { is_even(x) }, xs)  // [2, 4]
let rev     = reverse(xs)        // [5, 4, 3, 2, 1]
let first3  = take(3, xs)        // [1, 2, 3]
let rest    = drop(2, xs)        // [3, 4, 5]
let r       = range(4)           // [0, 1, 2, 3]
```

Polymorphic functions work with strings, records, and any element type:

```nula
import stdlib::list

// String arrays work with polymorphic functions
let excited = map(fn(s) { s + "!" }, ["a", "b", "c"])  // ["a!", "b!", "c!"]
let short   = filter(fn(s) { perform String.length(s) <= 4 }, ["hi", "hello", "hey"])  // ["hi", "hey"]

// Int-only numerical/ordering operations still reject incompatible element types.
```

---

## Module: test

Test assertions via the built-in `Test` effect. Assertion failures produce
runtime errors that abort execution and are reported by the `nula test` runner.

All functions are annotated `! {Test}`, meaning they can only be called from
contexts that handle the `Test` effect.

| Function | Signature | Description |
|---|---|---|
| `assert` | `fn assert(cond: Bool, msg: String) -> Unit ! {Test}` | Assert a boolean condition. On failure: `assertion failed: {msg}`. |
| `assert_eq` | `fn assert_eq(a: Int, b: Int) -> Unit ! {Test}` | Assert two integers are equal. On failure: `assertion failed: expected {b}, got {a}`. `a` = actual, `b` = expected. |
| `assert_true` | `fn assert_true(cond: Bool) -> Unit ! {Test}` | Assert a boolean condition is true. On failure: `assertion failed`. |
| `fail_with` | `fn fail_with(message: String) -> Unit ! {Test}` | Fail unconditionally with a message. Produces: `assertion failed: {message}`. |

### Usage

```nula
import stdlib::test

fn my_tests() {
    assert(1 + 1 == 2, "basic arithmetic failed")
    assert_eq(add(1, 2), 3)
    assert_true(meaning_of_life() == 42)
    // fail_with("not implemented yet")
}
```

---

## Module: fs

Filesystem I/O via the built-in `FS` effect. Operations are `perform FS.read`,
`perform FS.write`, `perform FS.append`, and `perform FS.exists`.

Effect annotations (`! {FS}`) mean callers must handle or propagate the `FS`
effect.

| Function | Signature | Description |
|---|---|---|
| `read` | `fn read(path: String) -> String ! {FS}` | Read entire file contents into a string. Returns nil if the file cannot be read. |
| `write` | `fn write(path: String, content: String) -> Unit ! {FS}` | Write a string to a file, overwriting existing content. Creates the file if needed. Returns nil on failure. |
| `append` | `fn append(path: String, content: String) -> Unit ! {FS}` | Append a string to the end of a file. Creates the file if needed. Returns nil on failure. |
| `exists` | `fn exists(path: String) -> Bool ! {FS}` | Check whether a file or directory exists at the given path. |

### Usage

```nula
import stdlib::fs

let content = read("data.txt")
write("output.txt", "Hello, world!")
append("log.txt", "another line\n")
let ok = exists("config.json")  // true or false
```

---

## Module: core

Function combinators and general-purpose utilities. All functions are pure.

| Function | Signature | Description |
|---|---|---|
| `identity` | `fn identity(x)` | Identity function. Returns its argument unchanged. |
| `const_fn` | `fn const_fn(x, _y)` | Constant function: always returns the first argument, ignoring the second. |
| `always` | `fn always(x)` | Given a value, returns a function that ignores its input and returns that value. |
| `apply` | `fn apply(f, x)` | Apply function `f` to argument `x`. |
| `compose` | `fn compose(f, g, x)` | Function composition (3-arg form): `compose(f, g, x)` = `f(g(x))`. |
| `compose_fn` | `fn compose_fn(f, g)` | Returns a new function `h(x) = f(g(x))`. |
| `flip` | `fn flip(f, x, y)` | Flip argument order: `flip(f, x, y)` = `f(y, x)`. |
| `negate` | `fn negate(b)` | Logical negation. |
| `clamp` | `fn clamp(x, lo, hi)` | Clamp `x` to the range `[lo, hi]`. |
| `between` | `fn between(lo, hi, x)` | True when `x` is between `lo` and `hi` (inclusive). |

### Usage

```nula
import stdlib::core

let id = identity(42)              // 42
let c = const_fn(1, "ignored")     // 1
let k = always(10)                 // fn(_y) { 10 }
let y = apply(fn(x) { x * 2 }, 5)  // 10
let z = compose(fn(x) { x + 1 }, fn(x) { x * 2 }, 3)  // 7 i.e. (3*2)+1
let f = compose_fn(fn(x) { x + 1 }, fn(x) { x * 2 })  // fn(x) { (x*2)+1 }
let w = flip(fn(a, b) { a - b }, 10, 3)  // -7 i.e. 3-10
let n = negate(true)               // false
let cl = clamp(15, 0, 10)          // 10
let b = between(0, 10, 5)          // true
```

---

## Module: option

`Option[T] = Some(T) | None` is defined once in the auto-imported prelude.
`stdlib::option` provides operations on that canonical type.

| Function | Description |
|---|---|
| `map(opt, f)` | Transform a present value. |
| `and_then(opt, f)` | Chain an Option-returning function. |
| `or_else(opt, f)` | Lazily recover from `None`. |
| `unwrap_or(opt, default)` | Return the value or an eager default. |
| `unwrap_or_else(opt, f)` | Return the value or compute a default lazily. |
| `filter(opt, pred)` | Keep a present value only if it satisfies the predicate. |
| `ok_or(opt, err)` | Convert to `Result[T, E]`. |
| `is_some(opt)` / `is_none(opt)` | Test the variant. |
| `unwrap(opt)` | Legacy aborting unwrap. Prefer total alternatives in portable code. |

---

## Module: result

`Result[T, E] = Ok(T) | Error(E)` is defined once in the auto-imported
prelude. `stdlib::result` provides operations on that canonical type.

| Function | Description |
|---|---|
| `map(r, f)` | Transform a successful value. |
| `map_err(r, f)` | Transform the error channel. |
| `and_then(r, f)` | Chain a Result-returning function. |
| `or_else(r, f)` | Recover from an error, optionally changing its type. |
| `unwrap_or(r, default)` | Return success or an eager default. |
| `unwrap_or_else(r, f)` | Return success or compute a fallback from the error. |
| `to_option(r)` | Discard the error and convert to `Option[T]`. |
| `is_ok(r)` / `is_err(r)` | Test the variant. |
| `unwrap(r)` | Legacy aborting unwrap. Prefer total alternatives in portable code. |

Higher-order collection operations such as `map2`, `sequence`, and
`traverse` remain in the official `result-ext` package rather than growing
the permanent standard library surface.

---

## Module: set

Integer set operations. Sets are represented as `[Int]` arrays with no
duplicates. Mutation functions return a new set; the original is unchanged.

> **Caveat**: `set` and `map` both export `empty`, `size`, `remove`, and
> `is_empty`. Importing both in the same file causes name collisions — import
> one at a time, or use distinct names via aliasing at the call site.

| Function | Signature | Description |
|---|---|---|
| `empty` | `fn empty()` | An empty set. |
| `contains` | `fn contains(set, value)` | Check whether a value is in the set. |
| `add` | `fn add(set, value)` | Add a value to the set (no-op if already present). Returns a new set. |
| `remove` | `fn remove(set, value)` | Remove a value from the set. Returns a new set. |
| `size` | `fn size(set)` | Number of elements in the set. |
| `is_empty` | `fn is_empty(set)` | True when the set is empty. |
| `union` | `fn union(a, b)` | Union: all elements in `a` or `b` (no duplicates). |
| `intersect` | `fn intersect(a, b)` | Intersection: elements present in both `a` and `b`. |
| `difference` | `fn difference(a, b)` | Difference: elements in `a` that are not in `b`. |

### Usage

```nula
import stdlib::set

let s = empty()                  // []
let a = add(s, 1)                // [1]
let b = add(a, 2)                // [1, 2]
let c = add(b, 1)                // [1, 2]  (no-op, already present)
let ok = contains(c, 2)          // true
let n = size(c)                  // 2
let u = union([1, 2], [2, 3])    // [1, 2, 3]
let i = intersect([1, 2], [2, 3]) // [2]
let d = difference([1, 2], [2])   // [1]
let r = remove(c, 1)             // [2]
```

---

## Module: map

Map operations over arrays of `{key, value}` records. Keys and values are inferred from use. Key lookup is O(n), and `get` returns `Option[value]` so absence is explicit.

> **Caveat**: `map` and `set` both export `empty`, `size`, `remove`, and
> `is_empty`. Importing both in the same file causes name collisions — import
> one at a time, or use distinct names via aliasing at the call site.

| Function | Signature | Description |
|---|---|---|
| `empty` | `fn empty()` | An empty map. |
| `insert` | `fn insert(k, v, m)` | Insert or update a key-value pair. Returns a new map. |
| `get` | `fn get(k, m) -> Option[V]` | Look up a key. Returns `Some(value)` or `None`. |
| `contains_key` | `fn contains_key(k, m)` | True when the key is present. |
| `remove` | `fn remove(k, m)` | Remove a key. Returns a new map. |
| `size` | `fn size(m)` | Number of key-value pairs. |
| `is_empty` | `fn is_empty(m)` | True when the map is empty. |
| `keys` | `fn keys(m)` | Return an array of all keys. |
| `values` | `fn values(m)` | Return an array of all values. |

### Usage

```nula
import stdlib::map

let m = empty()                    // []
let a = insert(1, 100, m)           // [{key: 1, value: 100}]
let b = insert(2, 200, a)           // [{key: 1, value: 100}, {key: 2, value: 200}]
let c = insert(1, 999, b)           // [{key: 1, value: 999}, {key: 2, value: 200}]
let v = get(1, c)                   // Some(999)
let miss = get(3, c)                // None
let ok = contains_key(2, c)         // true
let kk = keys(c)                     // [1, 2]
let vv = values(c)                   // [999, 200]
let d = remove(1, c)                 // [{key: 2, value: 200}]
let n = size(c)                      // 2
```

---

## Module: string

String operations built on the `String` builtin effect primitives
(`String.length`, `String.charAt`, `String.substring`, `String.concat`).
Functions use `perform` internally to access these effectful operations.

| Function | Signature | Description |
|---|---|---|
| `length` | `fn length(s)` | Length of the string in bytes. |
| `is_empty` | `fn is_empty(s)` | True when the string is empty. |
| `char_at` | `fn char_at(s, i)` | Character at index `i`, returned as a 1-character string. Returns `""` if out of bounds. |
| `char_code_at` | `fn char_code_at(s, i)` | Character code at index `i`, or `-1` if out of bounds. |
| `starts_with` | `fn starts_with(s, prefix)` | True when `s` starts with `prefix`. |
| `ends_with` | `fn ends_with(s, suffix)` | True when `s` ends with `suffix`. |
| `contains` | `fn contains(s, sub)` | True when `s` contains the substring `sub`. |
| `index_of` | `fn index_of(s, ch)` | First index of substring `ch` in `s`, or `-1` if not found. |
| `repeat` | `fn repeat(s, n)` | Repeat string `s` `n` times. |
| `trim` | `fn trim(s)` | Remove leading and trailing whitespace (spaces, tabs, newlines, CR). |
| `split` | `fn split(s, delim)` | Split string `s` by a delimiter character (a 1-char string). Returns an array of substrings. |
| `join` | `fn join(parts, sep)` | Join an array of strings with a separator string. |
| `replace` | `fn replace(s, from, to)` | Replace all occurrences of substring `from` with `to` in `s`. |

### Usage

```nula
import stdlib::string

let n = length("hello")                  // 5
let e = is_empty("")                      // true
let c = char_at("hello", 1)              // "e"
let cc = char_code_at("A", 0)            // 65
let sw = starts_with("hello", "he")       // true
let ew = ends_with("hello", "lo")         // true
let co = contains("hello", "ll")          // true
let i = index_of("hello", "l")            // 2
let r = repeat("ab", 3)                   // "ababab"
let t = trim("  hi  ")                    // "hi"
let parts = split("a,b,c", ",")           // ["a", "b", "c"]
let j = join(["a", "b", "c"], "-")         // "a-b-c"
let rep = replace("hello world", "o", "x") // "hellx wxrld"
```