# RFC 0021: Deterministic Compile-Time Evaluation

- **Status:** Draft
- **Tier:** Experimental
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-17
- **Resolved:** TBD
- **Language-version at effect:** N/A until accepted
- **Supersedes:** none
- **Superseded by:** none

## Summary

Add deterministic, effect-checked compile-time evaluation to Nulang without
introducing unrestricted AST macros.

The first version allows explicitly marked pure functions and module bindings
to execute during compilation. Compile-time execution uses ordinary Nulang
semantics, is bounded by deterministic resource limits, and rejects any effect,
capability, or operation that could observe ambient machine state.

The guiding rule is:

> Compile-time Nulang is ordinary pure Nulang evaluated early, not a second
> programming language and not an escape hatch into the compiler process.

The feature deliberately does **not** include syntax-generating macros,
filesystem access, environment variables, networking, wall-clock time,
randomness, subprocesses, FFI, actors, or compiler-internal AST mutation.

## Motivation

Nulang currently performs a few isolated compile-time decisions in parser or
lowering code. For example, an `ask` timeout accepts an expression
syntactically but the parser only captures it when it is a literal integer.
Similar needs will recur for:

- fixed timeout and retry parameters;
- static tables and lookup data;
- schema and protocol constants;
- generated finite state tables;
- precomputed numerical values;
- native representation metadata (RFC 0020);
- route/configuration metadata that must be known before code generation;
- specialization and static assertions.

Adding one bespoke "literal-only" path for each feature scales poorly.

Nim demonstrates the leverage of executing normal language code at compile
time, but unrestricted compiler-process execution and arbitrary AST mutation
would conflict with Nulang's goals around deterministic builds, durable
semantics, capability security, remote caching, and long-term format stability.

Nulang already has a stronger foundation than most languages for a safe design:

- algebraic effect rows can prove that a function is pure;
- authority/resource capabilities already classify external access;
- the HIR/MIR pipeline provides a backend-neutral executable subset;
- RFC 0019 Semantic IDs provide a natural cache key;
- the frozen Core subset provides a small deterministic semantic kernel.

## Design

### 1. No new keyword in Phase 1

RFC 0010 treats every reserved keyword as a permanent language tax. Phase 1
therefore uses the existing annotation mechanism:

```nula
@comptime()
fn fib(n: Int) -> Int {
    if n < 2 then n else fib(n - 1) + fib(n - 2)
}

@comptime()
let table = [fib(0), fib(1), fib(2), fib(3)]
```

`@comptime()` on a function means the function is eligible to execute in the
compile-time evaluator.

`@comptime()` on a module-level immutable `let` means its initializer must
be evaluated completely during compilation and replaced by a canonical
constant value.

Phase 1 does not allow `@comptime()` on mutable bindings.

The parser's current annotation representation is function-centric
(`FunctionAnnotation`). Implementation should rename/generalize this to
declaration annotation metadata rather than adding a parallel annotation type.

### 2. Evaluation happens after semantic checking

The order is:

```text
parse
  -> name/import resolution
  -> HM type checking
  -> effect inference/checking
  -> capability analysis
  -> comptime eligibility validation
  -> comptime evaluation
  -> HIR
  -> MIR
  -> backend
```

Compile-time code is never executed before its types and effects are known.

This prevents metaprogramming from becoming a pre-typechecker language with
different semantics.

### 3. Eligibility rule

A function may execute at compile time only if all of the following hold:

1. it is explicitly marked `@comptime()`;
2. its inferred concrete effect row is closed and empty;
3. it does not use actor/runtime-only operations;
4. it does not invoke FFI, Python, dynamic loading, process, environment,
   filesystem, network, time, random, database, inference, or system
   facilities;
5. all transitively called user functions are themselves eligible or are
   compiler-approved pure intrinsics;
6. every value crossing the evaluator boundary is a compile-time value;
7. no borrowed/runtime capability escapes into the evaluated result.

An explicit empty effect annotation is allowed but does not replace inference;
the implementation must verify the body rather than trust the declaration.

### 4. Compile-time values

Phase 1 supports canonical values composed from:

- Int;
- Float, subject to the same finite-value rules used by durable formats;
- Bool;
- String;
- Unit / Nil;
- tuples;
- records;
- variants whose payloads are compile-time values;
- arrays of compile-time values;
- opaque nominal wrappers whose underlying value is compile-time representable.

Actor references, closures as final values, runtime heap references, file
handles, foreign pointers, continuations, and effect handlers cannot escape a
compile-time expression.

A closure may exist transiently inside evaluation if its captures are themselves
compile-time values and the evaluator can represent it without embedding a
compiler-process pointer.

### 5. Evaluation engine

Phase 1 should use a dedicated deterministic evaluator over typed HIR or a
strict deterministic profile of the frozen Core interpreter.

It must **not** call `VM::run` with the normal runtime attached.

Preferred implementation boundary:

```text
typed AST/HIR
   |
   v
ComptimeEvaluator
   - immutable environment
   - deterministic value arena
   - explicit call stack
   - fuel counter
   - allocation budget
   - recursion-depth bound
   |
   v
ComptimeValue
```

The evaluator should share primitive semantic helpers with the VM where doing
so cannot expose runtime state. Arithmetic edge cases must match runtime
semantics exactly.

### 6. Deterministic resource limits

A compiler must not hang because a compile-time function does.

Every evaluation has explicit limits:

- instruction/evaluation fuel;
- maximum recursion depth;
- maximum aggregate allocation bytes;
- maximum produced string/aggregate size.

The exact defaults are implementation policy, not language semantics, but a
build must record the limits used in diagnostics/cache metadata.

Exhaustion is a compile error with an evaluation stack, not a fallback to
runtime execution.

Example:

```text
error: compile-time evaluation exceeded fuel limit
  table
    -> build_table(1000000)
       -> expand(999812)

help: reduce compile-time work or move this computation to runtime
```

### 7. Determinism contract

The evaluator has no ambient:

- filesystem;
- network;
- process API;
- environment variables;
- locale;
- system timezone;
- wall or monotonic clock;
- entropy source;
- thread scheduling;
- host pointer identity;
- nondeterministic hash-map iteration.

Collections visible to compile-time programs must use language-defined stable
iteration order or reject iteration when no stable order exists.

Floating-point evaluation must follow Nulang's specified semantics independent
of the host optimizer. If exact cross-target floating-point reproducibility
cannot be guaranteed for an operation, that operation is initially ineligible
for compile-time evaluation.

### 8. Effect checking is the primary sandbox

The effect system is not merely a convenience check; it is the compile-time
security boundary.

A function with any concrete effect is rejected:

```nula
@comptime()
fn read_config() -> String ! { FS } {
    perform FS.read("config")
}
```

Diagnostic:

```text
error: @comptime function 'read_config' is effectful
  inferred effects: {FS}
  compile-time functions require: {}

origin:
  FS <- read_config [body]
```

The effect provenance machinery proposed in PR #386 should back this
diagnostic so transitive impurity is explainable:

```text
error: @comptime function 'schema' is effectful
  Net <- schema -> load_schema -> fetch [body]
```

This is a direct example of compiler features reinforcing each other rather
than creating a separate comptime checker.

### 9. Capability restrictions

Reference capabilities still apply inside compile-time evaluation.

The evaluator never manufactures `iso`, `ref`, or foreign runtime ownership
from compiler-process memory. Compile-time aggregates are evaluator-owned
values that are serialized into canonical constants before normal lowering.

Linear values must obey the same exactly-once rules as runtime code.

A compile-time result therefore contains data, not ownership of evaluator
storage.

### 10. Module bindings

For:

```nula
@comptime()
let answer = expensive_pure_calculation()
```

the compiler:

1. type/effect/capability checks the initializer;
2. evaluates it;
3. canonicalizes the resulting `ComptimeValue`;
4. substitutes or lowers the binding as a normal immutable constant.

The source-level binding remains visible to tooling and diagnostics. The
compiler should not perform textual substitution.

### 11. Function calls

A `@comptime()` function remains callable at runtime unless a future
annotation explicitly requests compile-time-only visibility.

This avoids creating two function languages:

```nula
@comptime()
fn crc_table(poly: Int) -> [Int] { ... }

@comptime()
let prebuilt = crc_table(123)

// Also legal at runtime:
fn rebuild(poly: Int) -> [Int] {
    crc_table(poly)
}
```

The same function body must have the same pure semantics in both contexts.

### 12. Static arguments / partial evaluation are deferred

Phase 1 does not implicitly execute every call whose arguments happen to be
constants. Only an explicitly compile-time binding demands evaluation.

A later optimization may constant-fold pure calls automatically, but that is a
performance transformation rather than a language semantic requirement.

This separation keeps compilation cost predictable.

### 13. First compiler consumers

After the evaluator exists, migrate existing ad-hoc literal-only rules onto it.

The first target should be ask timeouts. Today:

```nula
ask worker.run() timeout 5000
```

captures the timeout only when the parsed expression is literally an integer.

After this RFC, compile-time bindings should work:

```nula
@comptime()
let default_timeout = 1000 * 5

ask worker.run() timeout default_timeout
```

Other static configuration surfaces should consume the same evaluator rather
than growing separate expression evaluators.

### 14. Cache model

Compile-time evaluation should be cacheable by semantic inputs rather than
source offsets.

Candidate cache key:

```text
(
    function SemanticId,
    canonical argument values,
    compiler semantic version,
    comptime evaluator version,
    target-independent numeric profile,
)
```

Target-specific values add a target profile to the key.

The evaluator's output is canonical serialized data, so Nulang Cloud can share
results across workers without trusting compiler heap snapshots.

### 15. Reproducibility

A build should be able to emit a comptime manifest containing:

- evaluated binding/function;
- semantic cache key;
- result hash;
- fuel consumed;
- allocation bytes;
- evaluator version.

This allows CI and remote build systems to diagnose cache misses and verify
reproducibility.

The manifest is tooling metadata, not part of runtime program semantics.

### 16. No AST macros in Phase 1

Phase 1 intentionally rejects Nim-style arbitrary AST macros.

Reasons:

- ASTs are compiler implementation details, not a long-lived language ABI;
- unrestricted AST construction makes formatting, tooling, source mapping,
  refactoring, and security substantially harder;
- macro expansion can bypass semantic abstractions that ordinary functions
  must respect;
- Nulang already has generics, typeclasses, algebraic effects, derives, and
  compile-time evaluation for most abstraction needs.

If syntax generation proves necessary, a later RFC should define **typed,
hygienic syntax values** with a stable language-level representation rather
than exposing Rust compiler AST structs.

### 17. Derives remain compiler transformations

Existing `@derive(...)` behavior should not be reimplemented as arbitrary user
macros in Phase 1.

Long term, derives may use a constrained typed reflection API, but their output
must remain hygienic and compiler-validated.

### 18. Reflection is deferred

A later phase may provide read-only compile-time reflection over stable semantic
descriptors:

```text
TypeInfo
FieldInfo
FunctionInfo
EffectInfo
```

Reflection must expose language semantics, not internal AST/HIR node shapes.

This would support serializers, schema generation, and bindgen without an
unstable macro API.

## Tier Classification

Experimental.

The feature adds compile-time semantics but does not alter frozen runtime
formats or the Core execution contract. The evaluator version is explicitly
separate from `VALUE_LAYOUT_VERSION`, bytecode version, and wire version.

If `@comptime()` metadata must persist in `.nbc` artifacts for linking,
that metadata addition must follow RFC 0001's versioning rules.

## Backwards Compatibility

Existing programs are unchanged.

`@comptime()` is opt-in. A compiler that does not understand the annotation
rejects it rather than silently running the binding at runtime.

Moving existing literal-only compiler features to the evaluator should be
strictly additive: every previously accepted literal remains accepted.

## Alternatives Considered

### Execute arbitrary Nulang in the normal VM during compilation

Rejected. The normal runtime can expose effects, actors, callbacks, clocks,
foreign code, and other ambient state. Compile-time execution needs a smaller
trust boundary.

### Nim-style unrestricted AST macros immediately

Rejected. The power-to-complexity ratio is poor for Nulang's current needs and
would create a second unstable API surface before deterministic evaluation is
established.

### Parser-only constant folding

Rejected as the general model. It duplicates language semantics before type
checking and quickly becomes a partial second interpreter.

### Implicitly evaluate every pure constant call

Deferred. It is a useful optimizer but makes compile cost less predictable and
is not required for language semantics.

### Add a `comptime` or `const` keyword now

Rejected for Phase 1. Existing annotations can express the feature without
permanently reserving another identifier. A keyword can still be proposed if
real-world use proves annotation syntax too cumbersome.

### Allow FS/env access behind explicit capabilities

Rejected for Phase 1. Even explicit access makes identical source produce
different outputs on different machines and complicates remote caching. A
future build-system layer can model declared build inputs separately from the
language evaluator.

## Open Questions

1. Should the first evaluator operate directly on typed AST, HIR, or frozen
   Core bytecode?
2. Which floating-point operations can promise bit-for-bit cross-target
   reproducibility in Phase 1?
3. Should `@comptime()` functions be exported across package boundaries in
   source form, semantic IR, or a dedicated evaluator artifact?
4. What default fuel/allocation limits give useful power while keeping editor
   feedback fast?
5. Should a future `@runtime_only()` annotation prevent accidental
   compile-time use of an otherwise pure function?

These do not change the central decision: compile-time execution is explicit,
pure, deterministic, bounded, and AST-macro-free in Phase 1.

## Resolution

Pending.
