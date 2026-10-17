# Nulang Example Programs

A curated suite of self-contained Nulang programs demonstrating the language
from basic to advanced. Example source is the authority; CI validates selected
smoke cases, but **the full catalog is not certified at every commit**. The
older example-suite verification snapshot referenced commit `fd516ef`.

## Running an example

Use a tagged release binary on your `PATH`:

```bash
nulang examples/08_actors.nula
```

Or build and run from this source checkout without assuming where Cargo places
the binary (the repository configures `CARGO_TARGET_DIR`):

```bash
cargo run --locked -- examples/08_actors.nula
cargo run --locked -- examples/18_inventory_reservations.nula
```

## A small real-world actor: inventory reservations

[`18_inventory_reservations.nula`](18_inventory_reservations.nula) models a
single stock-keeping unit. The `Inventory` actor owns `available` and
`reserved` counters. A `reserve` message checks the available balance before
mutating both values; an over-reservation is rejected. A `release` message
restores inventory.

Unlike a counter demo, this shows **why actor ownership matters**: business
invariants are enforced at the state owner, rather than relying on callers to
coordinate changes to shared mutable state.

The smoke test checks this message sequence:

```text
reservation accepted: 3
reservation rejected: 4 (insufficient stock)
available=2 reserved=3
release accepted: 1
available=3 reserved=2
```

After building the compiler, run
`NULANG_BIN=./target/debug/nulang bash scripts/verify_inventory_example.sh`
if the binary is at that path, or simply run
`bash scripts/verify_inventory_example.sh` to build into a known local target
directory and execute the test.

**Scope:** this is in-memory, single-process actor behavior. It does not
demonstrate persistence, durable reservation semantics, cross-node delivery,
exactly-once effects, authorization, or production readiness. For current
runtime boundaries see [implementation status](../docs/IMPLEMENTATION_STATUS.md).

**Note on example 17:** despite its historical filename,
[`17_actor_fetcher.nula`](17_actor_fetcher.nula) is a **sequential**
HTTP/JSON/FS program. Its source documents a current native-runtime
limitation combining actors and HTTP effects. Do not use it as evidence of
concurrent HTTP fetching.

## Examples

| # | File | Description | Command |
|---|------|-------------|---------|
| 01 | `01_hello.nula` | IO.print, string literals, basic expressions | `nulang examples/01_hello.nula` |
| 02 | `02_arithmetic.nula` | Let bindings, arithmetic, comparisons, booleans, type annotations | `nulang examples/02_arithmetic.nula` |
| 03 | `03_functions.nula` | Closures, multi-arg functions, recursion (factorial, fibonacci), blocks | `nulang examples/03_functions.nula` |
| 04 | `04_pattern_match.nula` | Match on literals, variants, tuples, records, guards, alias patterns | `nulang examples/04_pattern_match.nula` |
| 05 | `05_records.nula` | Record construction, field access, mutation, nested records | `nulang examples/05_records.nula` |
| 06 | `06_higher_order.nula` | Higher-order functions, composition, closure factories, recursion | `nulang examples/06_higher_order.nula` |
| 07 | `07_effects.nula` | Algebraic effects: perform, handle, String.length, Int.to_string | `nulang examples/07_effects.nula` |
| 08 | `08_actors.nula` | Actor declaration, spawn, message passing with `!`, behaviors with IO | `nulang examples/08_actors.nula` |
| 09 | `09_loops.nula` | While loops, for-in loops, break with/without values, nested loops | `nulang examples/09_loops.nula` |
| 10 | `10_pipe.nula` | Pipe operator `|>`, chaining transformations, closures in pipelines | `nulang examples/10_pipe.nula` |
| 11 | `11_arrays.nula` | Array literals, indexing, element mutation, array-based algorithms | `nulang examples/11_arrays.nula` |
| 12 | `12_json.nula` | JSON parse + stringify round-trip, field access via get_string/get_number | `nulang examples/12_json.nula` |
| 13 | `13_http.nula` | HTTP get and post requests via the Http built-in effect | `nulang examples/13_http.nula` |
| 14 | `14_option_result.nula` | Option/Result types, map, unwrap, catch, pattern matching | `nulang examples/14_option_result.nula` |
| 15 | `15_ranges.nula` | Range expressions, for-in-range loops, range with pipe/arithmetic | `nulang examples/15_ranges.nula` |
| 16 | `16_realworld.nula` | Real-world CLI: HTTP fetch, JSON parse, datetime, FS write, env, args | `nulang examples/16_realworld.nula <url>` |
| 17 | `17_actor_fetcher.nula` | **Sequential** URL reporter (legacy filename; no actor use), HTTP/JSON/FS | `nulang examples/17_actor_fetcher.nula` |
| 18 | `18_inventory_reservations.nula` | Actor-owned inventory invariants; successful reservation, rejection, release | `nulang examples/18_inventory_reservations.nula` |

## Notes

- Use `//` for line comments. `--` is NOT a standalone line comment (only valid
  after expressions on the same line, e.g. `let x = 1 -- inline`).
- The em dash character `—` (U+2014) is NOT accepted by the lexer.
- `let` bindings are immutable; use `var` for mutable state in loops and
  algorithms.
- Actors must appear before `let` bindings at the top level.
- `spawn Actor { ... }` field initializers use `=` (e.g. `count = 42`) and
  correctly override the actor's declared defaults.
- Top-level `fn` declarations with `match` on variant types may fail
  exhaustiveness checking when an `actor` block is present in the same file.
