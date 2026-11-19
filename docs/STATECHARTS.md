# Experimental state-machine transitions

The first XState-inspired statechart milestone extends the existing NuLang
state-machine actor declaration without introducing a separate executor.
This implementation is paired with draft RFC 0026; the new syntax is not yet
ratified as Stable.

## Event semantics

```nulang
state_machine Order {
    state Pending
    state Paid

    event pay(amount: Int) from Pending: Paid if amount > 0
    event reset: Pending
}
```

The first declared state is initial. The legacy event form without a source
is accepted in every state. An event with a source only transitions when that
state is active. The optional guard must evaluate to Bool and may contain
literals, event-parameter references, field reads, arithmetic, comparisons,
and Boolean operations. Calls, mutations, external effects, and variable
references outside the event parameters are rejected.

If source or guard disallows a transition, it does nothing and returns nil;
entry/exit hooks are not called. Accepted self-transitions retain existing
exit and entry semantics. Event names must remain unique within a machine.
This proposal does not yet implement nested states, parallel regions,
delays, repeated event names with distinct source states, or durable machines
as a separate execution type.

## Structural analysis

The Rust API `nulang::statechart::analyze_machine(&decl)` accepts an
already-parsed `Decl::StateMachine` and returns its initial state, state
list, unreachable states, one shortest event sequence for each reachable
state, and a Mermaid `stateDiagram-v2` string.

The analysis treats all guards as possibly true. A reachable state might
remain unreachable at runtime for particular guard values; generated paths
are graph witnesses, not executable tests or formal proofs of feasibility.
An unreachable state found by this overapproximation is structurally
unreachable under any guard outcome.

The static API introduces no runtime IO and does not run machine code.
Conformance tests exercise parser validation, effect-free guards, type
checking, formatting, AST lowering, graph traversal, and actual actor
execution.

## Machine CLI (experimental)

The CLI builds on the static analysis and generates **transition-edge coverage**
using shortest executable prefixes. It does not generate a new runtime.

```bash
nulang machine graph order.nula --format mermaid --out order.mmd
nulang machine graph order.nula --format json --out order-graph.json
nulang machine paths order.nula --fixtures fixtures.json --out order-paths.json
nulang machine test order.nula --fixtures fixtures.json
nulang machine fixtures order.nula --out fixtures.json
```

For multiple machines in one file, specify `--machine NAME`. Fixtures map
each event name to a JSON array of primitive argument values:

```json
{
  "pay": [42],
  "reset": []
}
```

`machine fixtures` suggests deterministic primitive fixture values. It
supports simple integer comparisons (e.g. `amount > 0` produces `1`),
direct boolean predicates, and primitive equality/inequality. Complex or
contradictory guards, unsupported types, and untyped arguments fail closed
and require manual fixtures. **Suggestions are not proofs:** verify them with
`machine test`, and review them before using in a trusted test environment.

## Integration with `nula test`

Place a machine in `tests/statecharts/order.nula` with its optional fixture file
`tests/statecharts/order.fixtures.json`. Running `nulang nula test` now
includes those machine tests alongside ordinary `tests/*.nula` tests in
the regular pass/fail summary and `--json` results. `--filter` selects
machine files by filename stem, and `--watch` detects changes to both
`.nula` files and `.fixtures.json` files.

This is **opt-in by file placement**: source files outside
`tests/statecharts/` are not automatically executed by the package
test runner. Machine hooks are still executed during these tests; only place
trusted, safe-to-run test machines in that directory. Missing fixtures
fail coverage rather than disappearing from the test summary.

The `paths` output lists executable cases, explicit skips, and structurally
unreachable states. Its shortest paths cover **transition edges** (including
a global event from each structurally reachable state) and negative cases
that verify source-scoped events are rejected from other reachable states. These are generated
test scenarios, not a proof that guards are satisfiable. Event arguments
come from supplied fixtures; missing, malformed, or unsupported fixtures
are skipped explicitly.

The `test` command recompiles the selected machine plus a generated
in-memory entry point for each case. A fresh bytecode actor and runtime
execute the sequence, and the resulting `_sm_state` is compared against
the expected target. It returns nonzero if any generated case fails, if
an edge is skipped, or if no executable cases exist. Guards that reject
their supplied fixture yield a failing case, not a passing test.

The current runner tests a selected machine independently, including its
entry/exit hooks. It **executes those hooks**, so run it only against trusted
source or mocked external effects. It does not compile arbitrary surrounding
application declarations or assert intermediate checkpoints. Fixture
arguments are limited to numbers, booleans, and strings. Large graphs are
bounded; no process-wide deployment/persistence guarantees are implied.

## Deferred work

Future versions may add event-family dispatch, hierarchical and parallel
states, typed invalid-event results, machine manifest metadata,
timer-triggered transitions, an inspection UI, richer guard fixture
synthesis, and persistent generated test artifacts. These require separate
semantic and compatibility review.
