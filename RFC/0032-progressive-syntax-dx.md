# RFC 0032: Progressive Syntax and Developer Experience

- **Status:** Draft
- **Tier:** Stable/Experimental guidance; no Frozen Core change in Phase 0
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-21
- **Depends on:** stabilization gate #231
- **Supersedes:** none

## Summary

Nulang should retain its current semantic power—HM inference, algebraic
effects, reference capabilities, actors, durability, supervision, distribution,
and portable execution—while making the ordinary source language substantially
smaller and easier to read.

The central rule is **progressive disclosure**:

> If the compiler can infer a mechanism safely, ordinary code should not have
> to spell that mechanism. Explicit syntax remains available where it changes
> semantics, documents a public boundary, or gives the programmer meaningful
> control.

This RFC separates syntax convergence from semantic change. Phase 0 is safe
during the active stabilization gate: make the formatter the canonical spelling,
preserve all semantic metadata when formatting, and align documentation and
examples. Later phases require their own implementation review and, where Stable
syntax changes, the normal deprecation contract.

## Motivation

The language has accumulated multiple spellings and several places where
implementation detail leaks into routine application code:

- local actor sends parse as both `send a msg()` and `a ! msg()`;
- actor construction is documented with both brace and call-like forms while the
  formatter already emits `spawn Actor(...)`;
- mutable bindings are `var`, but formatter paths have emitted non-language
  spellings such as `let mut`;
- `perform` is currently required for both genuinely external effects and
  operations that are conceptually pure conversions or collection helpers;
- `!` carries unrelated meanings across actor sends, error types, effect rows,
  and unary negation;
- capability vocabulary exposes the full Pony-inspired lattice even when the
  compiler can infer a safe capability;
- historical `catch`/`fail` syntax overlaps with `Result`, `?`, algebraic
  effects, and actor supervision;
- workflow/agent orchestration has language-level syntax despite actors + effects
  being the more durable semantic substrate.

The problem is not insufficient expressiveness. It is **surface-area entropy**.

## Design principles

### 1. One canonical spelling

The parser may accept compatibility syntax during migrations, but `nulang fmt`
MUST emit one canonical spelling for every AST construct. Documentation and
examples MUST teach that spelling first.

Alternative spellings are compatibility affordances, not co-equal language
design.

### 2. Preserve semantic distinctions that matter

Syntax should remain visibly different when operations have materially different
semantics.

Examples:

- ordinary function call: `f(x)`;
- local asynchronous actor send: `worker ! run(x)`;
- synchronous actor request/reply: `ask worker value()`;
- explicitly forced distributed delivery: `send remote worker run(x)` /
  `ask remote worker value() timeout 1000`.

The formatter MUST never erase transport, timeout, authority, ownership,
durability, or error semantics.

### 3. Infer ceremony; expose control

Effect rows, capabilities, and generic types should be inferred by default.

Explicit forms remain useful for:

- public API contracts;
- security/authority boundaries;
- generic higher-order code;
- FFI/WIT boundaries;
- durable/replay-sensitive operations;
- cases where more than one safe semantic choice exists.

### 4. Runtime primitive does not imply language effect

A VM intrinsic may be implemented through the same host dispatch machinery as an
effect without being conceptually effectful.

Pure operations such as integer/string conversion, string length, and collection
length should ultimately have ordinary pure APIs. External observation or
mutation—filesystem, network, time, process execution, secrets, inference,
storage—remains effectful.

### 5. Actors remain explicit

Nulang should not make actor messaging look like an ordinary method call.
Latency, ordering, isolation, failure, and distribution make the actor boundary
semantically important.

The concise `!` spelling is therefore a feature, not noise.

## Canonical source policy

Phase 0 adopts the formatter as the executable canonical-source contract.

### Bindings

Canonical:

```nulang
let immutable = 1
var mutable = 0
```

Do not emit Rust-style `let mut`.

### Conditionals

Until a later syntax RFC changes Stable grammar, canonical formatting remains:

```nulang
if condition then value else other
```

The parser may continue to accept currently supported block variants. Docs should
not present multiple forms as equally canonical.

### Pattern matching

Canonical:

```nulang
match value {
    | Some(x) => x
    | None => 0
}
```

### Actor construction

Canonical:

```nulang
let worker = spawn Worker()
let configured = spawn Worker(retries = 3)
```

Brace construction can remain parser compatibility while migration data is
collected.

### Actor messaging

Canonical local send:

```nulang
worker ! run(job)
```

Explicit remote send remains keyword-based because the transport choice is part
of the semantics:

```nulang
send remote worker run(job)
```

Request/reply remains:

```nulang
let state = ask worker state()
let remote = ask remote worker state() timeout 1000
```

### Strings

The already-implemented f-string form is the preferred interpolation spelling:

```nulang
f"Worker {id} processed {count} items"
```

Legacy `#{...}` interpolation may remain accepted for compatibility but the
formatter should converge on f-strings.

## Later phases

The following are design targets, not Phase-0 parser changes.

### Phase 1 — error model convergence

Complete RFC 0015:

- expected failure: `Result[T, E]`;
- propagation: `?`;
- optionality: `Option[T]`;
- actor/runtime faults: supervision;
- remove `catch` and `fail` after the deprecation window;
- stop using `nil` as an error sentinel in public stdlib APIs.

Prefer readable signature sugar such as:

```nulang
fn load(id: Id) -> User throws LoadError
```

over adding more meanings to `!`.

### Phase 2 — pure primitive cleanup

Reclassify conceptually pure built-ins away from algebraic effects and expose
ordinary APIs, ideally method-call syntax where type-directed resolution is
unambiguous:

```nulang
name.len()
id.to_string()
items.len()
```

This must not weaken effect checking for external operations.

### Phase 3 — effect syntax simplification

Evaluate making effect operations use ordinary call syntax while retaining
compiler-owned effect inference:

```nulang
IO.print("hello")
Http.get(url)
```

instead of requiring `perform` at every call site.

Explicit `perform` may remain available for user-defined handler-heavy code if
it provides genuine readability or disambiguation value.

Public boundaries should use a readable explicit effect contract (working name
`uses`) while private/internal functions infer rows:

```nulang
pub fn serve() uses Net, Storage {
    ...
}
```

Exact syntax requires a follow-up Stable-tier RFC.

### Phase 4 — capability progressive disclosure

Keep the full capability lattice in the type system, but investigate a smaller
beginner-facing vocabulary plus inference.

Possible intent-level surface:

- `read` — immutable borrowed/readable;
- `mut` — mutable local access;
- `own` — unique/transferable ownership.

The compiler maps these intents to the strongest valid underlying capability and
continues to expose exact `iso/trn/ref/val/box/tag/lineariso` controls for
systems code.

This is not permission to weaken sendability or linear-consumption checks.

### Phase 5 — shrink permanent keyword surface

Continue the RFC 0010 direction:

- prefer actors + libraries over permanent `agent` orchestration syntax;
- prefer actors + durable libraries over a growing workflow mini-language;
- move transient domain vocabulary out of the Frozen/Stable keyword set;
- keep language keywords for concepts expected to remain meaningful for decades.

## Tooling requirements

Before any later phase becomes Stable:

1. parser compatibility fixtures for old and new spelling;
2. formatter migration to the new canonical output;
3. formatter idempotence;
4. parse(format(parse(source))) semantic equivalence tests;
5. documentation snippets executed in CI;
6. LSP completion/hover updated in the same change;
7. `nulang migrate` support for source-breaking rewrites;
8. a deprecation period appropriate to the affected stability tier.

## Phase-0 acceptance criteria

- [ ] mutable bindings format back to valid `var` syntax;
- [ ] local sends format canonically as `!`;
- [ ] explicit remote sends retain `remote`;
- [ ] asks retain both `remote` and literal timeout metadata;
- [ ] formatter output reparses and is idempotent;
- [ ] syntax docs describe formatter output as canonical;
- [ ] examples use call-like spawn syntax where possible;
- [ ] no parser/runtime/effect/capability semantics change in this phase.

## Non-goals

Phase 0 does **not**:

- remove any Stable syntax;
- add new keywords;
- change effect inference;
- change capability safety;
- alter actor scheduling or delivery;
- alter persistence, replay, bytecode, or wire formats;
- bypass stabilization gate #231.

The purpose of Phase 0 is to stop syntax drift and make later simplification
deliberate rather than incremental.
