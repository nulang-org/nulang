# RFC 0031: Effect-Aware Execution Classes

- **Status:** Draft — Phase 1 implemented
- **Tier:** Experimental runtime metadata; no source syntax change
- **Created:** 2026-09-20
- **Depends on:** RFC 0017 (unified runtime primitives)

## Summary

Nulang already knows which effects a program may perform. The runtime should use that semantic information to decide **how work executes**, instead of treating every host effect as arbitrary synchronous code.

Phase 1 introduces a provider-neutral execution classification:

```text
Inline
CooperativeSuspend
AsyncExternal
BlockingHost
CpuBound
Accelerator
HandlerDefined
```

The classifier is metadata only in this phase. It does not change VM opcodes, scheduling, source syntax, or effect semantics.

## Motivation

A cooperative actor scheduler is only as responsive as its slowest inline operation. Today Nulang already handles `Inference.ask` specially by suspending the actor and dispatching work to a persistent worker thread, but other host effects such as Python, filesystem, process, and system operations can still represent blocking work.

The target architecture is:

```text
effect + operation
       ↓
execution class
       ↓
runtime execution policy
       ├─ inline actor scheduler
       ├─ cooperative suspension
       ├─ async I/O executor
       ├─ blocking/foreign executor
       ├─ CPU compute pool
       └─ accelerator executor
```

This is analogous to BEAM's separation of scheduler-safe work from dirty work, but Nulang can derive the policy from language/runtime semantics rather than asking users to configure dispatchers manually.

## Classes

### Inline

Cheap non-blocking runtime work such as actor bookkeeping, pure container/string operations, local CRDT updates, random-number generation, and metadata access.

### CooperativeSuspend

Runtime-owned waits where the actor should yield without occupying a scheduler thread. Examples: `Timer.sleep`, `Signal.wait`, and receive waits.

### AsyncExternal

External work that should run asynchronously while the actor is suspended. Current examples include HTTP/network calls, inference, realtime network operations, and future async database providers.

### BlockingHost

Host operations that may block an OS thread and therefore must not execute on cooperative actor scheduler threads once Phase 2 is wired. Conservative examples: filesystem, Python/FFI, process/system operations, and terminal I/O.

### CpuBound

Long-running CPU work that should use a compute pool with explicit budgeting rather than monopolize an actor scheduler thread.

### Accelerator

GPU/TPU/etc. work associated with a resource-aware executor. This class complements actor resource placement but does not name any concrete accelerator or cloud provider.

### HandlerDefined

User-defined or unknown effects. The runtime must not assume scheduler safety until the installed handler supplies an execution contract.

## Phase 1 implementation

- Add `EffectExecutionClass` to the canonical runtime primitives layer.
- Add semantic `classify_effect_execution(Effect, op)`.
- Add named built-in classification including aliases/runtime-only namespaces.
- Add `requires_isolation()` and `suspends_actor()` helpers.
- Expose `BuiltinOp::execution_class()` through the standard-library inventory.
- Fail closed for unknown named effects as `HandlerDefined`.
- Add a registry test requiring every built-in operation to have an explicit class.

## Phase 2 — isolated blocking executor

Introduce a bounded blocking/foreign executor:

```text
actor performs BlockingHost effect
        ↓
capture resumable VM state
        ↓
bounded executor queue
        ↓
worker executes host operation
        ↓
completion event
        ↓
actor resumes
```

Requirements:

- bounded queue and explicit overload response;
- cancellation/deadline support;
- panic isolation;
- actor/node shutdown cleanup;
- per-tenant/resource quotas in Nulang Cloud;
- no raw actor heap pointers crossing worker threads;
- replay/durability policy defined separately from scheduling policy.

Python should be the first Phase-2 target because it is already an explicit foreign boundary and is currently a clear scheduler-stall risk.

## Phase 3 — CPU and accelerator executors

Add explicit CPU-budget and accelerator-aware execution. Resource requirements should lower into the actor placement model rather than embedding cloud providers in the language.

## Phase 4 — compiler/runtime optimization

Once execution classes are trustworthy, the compiler/runtime may use them for:

- automatic effect batching;
- parallel execution of independent async effects;
- scheduler admission control;
- resource-aware actor placement;
- backpressure propagation;
- tracing/latency attribution;
- static warnings when an effect would block a latency-sensitive actor.

## Compatibility

This RFC does not alter the stable effect row, capability system, source syntax, bytecode format, or current runtime behavior in Phase 1. It adds metadata so future runtime changes can be introduced behind explicit compatibility gates.

## Non-goals

- Do not infer that all user-defined effects are safe inline.
- Do not expose thread-pool names or cloud-provider concepts as language syntax.
- Do not equate execution class with durability semantics; `EffectBoundary` remains a separate concept.
- Do not move work across threads until value/ownership boundaries are proven safe.