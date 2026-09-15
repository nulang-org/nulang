---
title: Signals, Timers & Queries
description: Workflow suspension and resume primitives — signals, durable timers, non-blocking LLM calls, and read-only queries.
---

## Suspension and Resume

Workflow steps can suspend waiting for external events. When a step suspends, the runtime persists the suspension marker to the journal and frees the actor. On resume, the runtime restores the step's execution state and continues. All four primitives below are durable — they survive node restarts.

## Signals

`perform Signal.wait(name)` suspends the workflow step until the named signal arrives:

```nulang
workflow Signaled {
    step wait_for_go {
        perform Signal.wait("go")
    }
}
```

When the step executes `Signal.wait("go")`, the actor suspends and records the signal name in the journal. Delivering the signal (`rt.signal_workflow(actor_id, "go", None)`) resumes the step, which continues past the `perform`.

### Chained signal waits

A step that waits on two signals in sequence suspends twice:

```nulang
workflow TwoSignals {
    step wait_for_both {
        (perform Signal.wait("first"), perform Signal.wait("second"))
    }
}
```

The step suspends on `"first"`, resumes when it arrives, then suspends again on `"second"`. Both suspension markers are journaled, so a restart between the two waits resumes correctly — the second wait is re-captured, not dropped.

### Signal replay on restart

After a node restart, signals that were delivered but not yet processed are replayed from the journal. The recovered workflow re-arms the wait and the step completes when the replayed signal arrives.

## Timers

`perform Timer.sleep(name, duration_ms)` schedules a durable workflow timer:

```nulang
workflow TimerWorkflow {
    step wait { perform Timer.sleep("timeout1", 1) }
}
```

The timer is journaled with its name and duration. The actor suspends. When the timer fires, the runtime resumes the step.

### Timer replay on restart

On recovery, durable timers are re-armed from the journal. If the timer should have already fired (the duration elapsed during the downtime), it fires immediately on recovery. This guarantees a workflow never gets stuck waiting for a timer that already expired.

## LLM Calls

`perform Inference.ask(prompt)` sends the prompt to the configured LLM and suspends the step until the response arrives (the legacy `LLM.ask` spelling dispatches identically):

```nulang
workflow LlmFlow {
    step ask_step { self.answer = perform Inference.ask("hello") }
}
```

Unlike a blocking `ask` to an agent, `Inference.ask` inside a workflow step suspends non-blockingly. The runtime spawns a worker thread for the HTTP call, the actor suspends, and the step resumes when the LLM response arrives.

### LLM + signal chaining

A step can perform `Inference.ask` and `Signal.wait` in sequence:

```nulang
workflow SignalThenLlm {
    step wait_then_ask {
        (perform Signal.wait("go"), self.answer = perform Inference.ask("hello"))
    }
}
```

The step suspends on the signal, resumes when it arrives, then suspends again on the LLM call. Both suspension markers are journaled. If the node restarts between the signal and the LLM call, the recovered step re-issues the LLM call and completes normally.

### Cost tracking

When the agent or workflow has a `pricing` configuration, LLM calls record token costs for usage tracking. The cost is written back to the agent's memory subsystem after the call completes.

## Queries

`perform Workflow.query(self, "name")` invokes a registered query handler on the workflow actor:

```nulang
workflow Counter {
    step bump { self.step_index = self.step_index + 1 }
    step inspect { self.observed = perform Workflow.query(self, "progress") }
}

fn progress() -> Int { self.step_index }
```

Query handlers are plain functions that read `self` state. The runtime invokes the handler with the workflow actor bound as `self`, so it observes the actor's current state without mutating it.

### Query properties

- **Read-only**: queries do not append workflow events or advance `step_index`.
- **Unknown names return `nil`**: querying an unregistered name returns `nil`, not an error.
- **Runs on a private VM**: the query handler executes on a separate VM instance, so it cannot disturb the step's own execution state.

### Registering query handlers

Query handlers are registered programmatically via the runtime API:

```rust
rt.register_workflow_query(actor_id, "progress", handler);
let result = rt.query_workflow(actor_id, "progress");
```

The `handler` is a function value (a function-table index) returned from the program entry. In typical usage, the program entry returns the handler function and the host registers it before invoking queries.

## Built-in Effect Summary

| Effect | Operation | Signature | Description |
|--------|-----------|-----------|-------------|
| `Signal` | `wait` | `wait(name: String) -> Unit` | Suspend until the named signal arrives |
| `Timer` | `sleep` | `sleep(name: String, duration_ms: Int) -> Unit` | Schedule a durable workflow timer |
| `Inference` | `ask` | `ask(prompt: String) -> String` | Non-blocking LLM call; suspends until response |
| `Workflow` | `query` | `query(self, name: String) -> Value` | Read-only state query via registered handler |

These operations are only available inside workflow actors (runtime-host context). Outside a workflow, they are nil no-ops.

## Next

- [Durable Workflows Overview](/workflows/overview/) — workflow declarations, steps, and saga compensation
- [Standard Library Overview](/stdlib/overview/) — all built-in effects
