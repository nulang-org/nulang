---
title: Durable Workflows Overview
description: Workflow declarations with steps, parallel branches, event emission, and saga compensation — checkpointed for crash recovery.
---
## Workflows

A workflow is a persistent actor with checkpointed state that progresses through named steps. On the reference bytecode runtime, workflows survive node restarts: each step completion is journaled, so a crashed workflow resumes from its last completed step rather than restarting from scratch.

> **Backend status:** durable workflow semantics are currently implemented by the
> reference bytecode/runtime path, but the canonical portable/cloud WASM backend
> deliberately rejects `workflow` declarations. WASM admission fails closed until
> it can preserve the same step journal, signal/timer suspension and resume, crash
> recovery, and saga-compensation semantics. This prevents a partially lowered
> workflow from being mistaken for a durable Cloud workload. For today's portable
> Cloud path, model the durable unit as an actor and use the supported durable
> state/effect contracts; do not assume local bytecode workflow behavior implies
> WASM deployment parity.

## Declaring a Workflow

```nulang
workflow PurchaseOrder {
    step validate {
        // Step body: runs when the step executes
        self.step_index = self.step_index + 1
    }
}
```

Each `step` becomes a persistent actor behavior. The runtime tracks `step_index` automatically — it starts at 0 and advances by 1 after each step completes. **Workflow progress is durable, but user fields are not durable merely because they are assigned through `self`.**

### Explicit workflow state (draft implementation)

The native bytecode implementation in [PR #1550](https://github.com/nulang-org/nulang/pull/1550) proposes explicit user-state declarations:

```nulang
workflow Summarize {
    state durable answer: String = ""
    state local attempts_in_process: Int = 0

    step ask {
        self.answer = perform Inference.ask("Summarize this report")
        self.attempts_in_process = self.attempts_in_process + 1
    }
}
```

- **`state durable`** belongs to the compiler-owned actor schema and is included in committed snapshots.
- **`state local`** exists only in the current runtime process; it is intentionally excluded from checkpoints.
- Initializers currently must be typed literals, and each field must explicitly select `durable` or `local`. Internal fields such as `step_index` cannot be redeclared.
- An **undeclared** `self.field` assignment is not automatically persisted. Declare every user field whose value must survive recovery.
- Inference requests and other external effects may be **re-dispatched** after an interrupted activation. These declarations do not provide exactly-once provider execution.

This addition is **not yet released or verified by terminal-green CI**. Until PR #1550 and its stacked crash tests pass, treat it as an in-progress language/runtime contract, not an available production guarantee.

## Steps and Parallel Branches

A workflow body contains `step`, `parallel`, and `compensate` blocks:

```nulang
workflow ParallelTest {
    step before { (emit BeforeDone(), self.step_index = self.step_index + 1) }
    parallel {
        step branch_a { emit BranchA_Done() }
        step branch_b { emit BranchB_Done() }
    }
    step after { emit AfterDone() }
}
```

- **`step name { body }`** — a sequential step. Runs after the previous step completes.
- **`parallel { step ... step ... }`** — branches run concurrently. All branches must complete before the workflow continues to the next sequential step.
- **`compensate { body }`** — saga compensation. Runs if a step fails (see below).

## Event Emission

Steps can emit durable events for event-sourcing. Emitted events are journaled and replayed on recovery:

```nulang
workflow Counter {
    step start { (emit Started(0), self.step_index = self.step_index + 1) }
    step second { (emit Incremented(1), self.step_index = self.step_index + 1) }
}
```

`emit EventName(args)` appends an event to the workflow's journal. On restart, emitted events are replayed so downstream consumers (queries, monitors) see consistent state.

## Saga Compensation

The `compensate` block runs when a step fails, rolling back partial work:

```nulang
workflow SagaTest {
    step a {
        (self.step_index = self.step_index + 1, self.a_done = 1, emit A_Done())
    } compensate {
        // Runs if step 'a' fails — undo the work
        self.a_done = 0
    }
}
```

Compensation follows the saga pattern: each step can have a compensate block, and on failure the runtime runs the compensation for all completed steps in reverse order. This ensures partial work is undone rather than left in an inconsistent state.

## Spawning a Workflow

Spawn a workflow like an actor:

```nulang
workflow PurchaseOrder { step validate { 1 } }

let w = spawn PurchaseOrder {} in { w }
```

`spawn WorkflowName {} in { ... }` creates a running workflow instance. The runtime persists its initial state and begins executing from step 0.

## Crash Recovery

Workflows are durable: a node restart does not lose progress. The runtime:

1. **Checkpoints** after each step completion (state + `step_index` written to the persistence store).
2. **Journals** emitted events and suspension markers (signal waits, timer waits, LLM calls).
3. **Replays** the journal on recovery, restoring the workflow to its last checkpointed state.

After recovery, the workflow resumes from its last completed step. Any in-flight suspension (signal wait, timer, LLM call) is re-armed from the journal and re-driven when the awaited event arrives.

### Example: survive a restart

A two-step workflow emits an event in each step. After step 1 completes, simulate a restart by loading the workflow into a fresh runtime sharing the same persistence store — the workflow resumes at `step_index = 1` and runs step 2, completing normally.

## Queries

Workflows expose read-only state through registered query handlers:

```nulang
workflow Counter {
    step bump { self.step_index = self.step_index + 1 }
}

fn progress() -> Int { self.step_index }

let c = spawn Counter {} in { progress }
```

The `progress` function, returned from the program entry, becomes a query handler. Query handlers run read-only on the workflow's current state — they do not append events or advance `step_index`. See [Signals, Timers & Queries](/workflows/signals-timers/) for the query API.

## Next

- [Signals, Timers & Queries](/workflows/signals-timers/) — suspension/resume primitives for workflow steps
- [AI Agents Overview](/ai/overview/) — agents that workflows can orchestrate

> **Note**: The `workflow` keyword is currently Experimental and is proposed for deprecation in favor of plain `actor` declarations that import `nlc.workflow` (RFC 0004). The keyword remains functional and will continue to work through at least two major language versions.
