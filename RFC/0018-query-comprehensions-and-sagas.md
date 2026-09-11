# RFC 0018: Query Comprehensions & Reversible Sagas

- **Status:** Accepted
- **Tier:** Experimental
- **Author:** Nulang Core Team
- **Created:** 2026-08-27
- **Resolved:** 2026-08-27
- **Language-version at effect:** 

## Summary

Introduces language-level query comprehensions (inspired by C# LINQ) and reversible distributed transactions (Sagas, inspired by Janus/Eel). These features leverage Nulang's existing algebraic effects, capability system, and event-sourced state models to provide unified, backend-agnostic querying over distributed data (CRDTs, SQL) and type-safe orchestration of multi-actor workflows with automatic compensations.

## Motivation

Querying distributed state (CRDTs, Turso SQL) currently requires ad-hoc, low-level iteration or strings embedded in network calls. Conversely, coordinating multi-step actor workflows requires manually writing error handling and compensation logic, which is tedious and error-prone. By unifying these under language-level constructs, Nulang can provide:
1. Declarative, type-safe data access over disparate backends (local iterators, distributed CRDT sets, external libsql databases).
2. Bulletproof distributed transactions (Sagas) where the type system mathematically guarantees that any side-effect performed inside a transaction has an associated, typed compensation action.

## Design

### Query Comprehensions (LINQ)

**Syntax**
Introduce a comprehension grammar production, inspired by ML and C#:
```nulang
expr_query ::= 'from' pat 'in' expr query_body
query_body ::= query_clause* 'select' expr
query_clause ::= 'where' expr
               | 'join' pat 'in' expr 'on' expr '==' expr
```

**Lowering to Effects**
The comprehension desugars into standard effect invocations on a built-in `Query` effect:
```nulang
from user in users
where user.age > 18
select user.name
```
Desugars to:
```nulang
perform Query.select(
  perform Query.filter(users, fn(user) user.age > 18),
  fn(user) user.name
)
```
(Or similar chaining of operations, building an AST of the query operations).

**Execution Handlers**
The `Query` effect allows different execution backends based on the target collection type, handled dynamically via Nulang's algebraic effect handlers:
- **Local:** Iterates over in-memory collections (`List`, local CRDT state) using standard iterators.
- **CRDTs:** Distributes filter predicates (compiled to MIR closures or serialized ASTs) to remote replicas holding CRDT state, minimizing data transfer across the cluster.
- **Turso/SQL:** Translates the comprehension AST into SQL queries (`libsql`) at compile-time or runtime, passing parameterized queries to the `SqliteStore` persistence backend.

### Reversible Sagas (Janus/Eel)

**Syntax**
Introduce a `saga` expression block:
```nulang
saga {
   // reversible steps
} else {
   // optional manual fallback if compensation ultimately fails
}
```

**Mechanism & Effect Typing**
To guarantee reversibility, any algebraic effect performed within a `saga` block must carry a `Reversible` capability constraint, or the user must provide an explicit compensation handler block (`undo`).
```nulang
saga {
   let order_id = perform Payment.charge(amount) 
       undo { perform Payment.refund(order_id) }; // Explicit compensation
   
   // If `Inventory.reserve` fails, the `undo` block above is automatically triggered.
   perform Inventory.reserve(item);
}
```

**Automatic Micro-Reversibility (PisoLang ILNF)**
While external effects like `Payment.charge` require manual `undo` blocks (macro-reversibility), pure function calls and in-memory state transformations inside a `saga` block can be *automatically* reversed. Inspired by PisoLang (a reversible functional language), Nulang's MIR lowering pipeline will elaborate nested pure function applications into an **Invertible Let-Normal Form (ILNF)**.

For example, a pure transformation `let y = g(f(x))` inside a saga is elaborated to:
```nulang
// Forward (ILNF)
let temp = f(x)
let y = g(temp)

// Backward (automatically derived on rollback)
let temp = g_inv(y)
let x_restored = f_inv(temp)
```
This "micro-reversibility" allows the Nulang VM to automatically run pure local state transformations backwards upon a saga failure. Manual `undo` blocks are therefore only strictly required for side-effects and network boundaries, drastically reducing developer boilerplate.

Nulang's `EventSourced` state models integrate natively with this construct. Event-sourced actors emit domain events upon state changes; within a `saga`, the system can transparently issue compensating commands based on the inverse of those events.

The `CapabilityAnalyzer` (in `src/effect_checker.rs`) will be extended to ensure that no irreversible, uncompensated effects (like arbitrary network I/O, `IO.print`, etc.) are performed inside the `saga` block.

## Tier Classification

Experimental. This introduces syntax extensions to the parser (`src/parser.rs`), AST nodes (`src/ast.rs`), type/effect checking rules (`src/effect_checker.rs`), and standard library additions (`Query` effect).

## Backwards Compatibility

Since this adds new syntax (`from`, `select`, `where`, `saga`, `undo`), existing identifiers using these words will break if they become strict keywords. 

**Mitigation:** We can introduce them as contextual keywords (e.g., `from` is only a keyword at the start of an expression, `saga` is a block identifier) or require a language edition bump. The bytecode format (`.nbc`) requires no changes, as comprehensions desugar to function/effect calls and `saga` lowers to standard try/catch/compensation handler machinery (using continuations via `Continuation` deep clones).

## Alternatives Considered

- **Macro-based approach:** Implementing comprehensions as macros. Rejected because Nulang currently lacks a procedural macro system, and integrating with the typechecker for SQL AST generation requires deep compiler knowledge.
- **Native SQL Syntax:** Baking raw SQL strings natively into the VM. Rejected because it violates the actor-encapsulation model and doesn't work for CRDTs or local collections.
- **Manual Sagas:** Continuing to rely on manual state machines and effect handlers. Rejected due to high developer burden and boilerplate for multi-actor workflows.

## Resolution

Accepted on 2026-08-27. The previously open architectural questions were resolved as follows to perfectly align with Nulang's core constraints:

1. **AST Reification for CRDTs:** Instead of serializing closures (which violates `packet_payload_wire_safe`), `Query.filter` predicates will be reified at compile-time into a serializable AST (MIR fragment). Captured variables will be passed explicitly as by-value arguments.
2. **Saga Compensation Escalation:** `undo` blocks are allowed to perform effects. If a compensation fails, the actor crashes with a `SagaCompensationFailed` fault. Because sagas are journaled to the `PersistenceStore`, the Supervisor will read the durable log and retry the compensation (via standard OTP-style escalation) until it succeeds or the node is safely shut down.
3. **Hybrid Snapshotting vs. ILNF:** Statically guaranteeing injectivity for all pure functions is too restrictive. The compiler will apply ILNF for zero-allocation reversals where it can trivially prove injectivity. For irreversible pure operations (like `let x = 0`), it falls back to a cheap memory snapshot of the actor's 64KB `ActorHeap` (resetting the bump allocator pointer on rollback).
