---
title: Supervision Trees
description: OTP-style supervision trees for fault-tolerant actor systems — strategies, child policies, and cascading shutdown.
---

## Fault Tolerance with Supervisors

Nulang implements Erlang/OTP-style supervision trees. A **supervisor** monitors child actors and applies a restart strategy when a child exits abnormally.

## Creating a Supervisor

```nulang
let sup_id = perform Otp.create_supervisor("my_sup", 0)
```

The second argument is the strategy:

| Value | Strategy | Behavior |
|-------|----------|----------|
| `0` | `one_for_one` | Restart only the failed child |
| `1` | `one_for_all` | Restart all children on any failure |
| `2` | `rest_for_one` | Restart failed child + all children started after it |
| `3` | `simple_one_for_one` | All children are instances of a single template |

## Adding Children

Place an existing actor under supervision:

```nulang
perform Otp.supervise_child(sup_id, worker_actor, 0)
```

Child restart policies:

| Value | Policy | Description |
|-------|--------|-------------|
| `0` | `permanent` | Always restarted on exit |
| `1` | `temporary` | Never restarted |
| `2` | `transient` | Restarted only on abnormal exit |
| `3` | `respawn_on_node_loss` | Re-spawn the child on another node when its home node is confirmed gone (RFC 0014). Implies `permanent` semantics for the local exit protocol, plus shadow replication and directory registration. Only meaningful for durable children. |

## Simple One-for-One Templates

For `simple_one_for_one` supervisors, set a child template:

```nulang
let sup = perform Otp.create_supervisor("pool", 3)  // simple_one_for_one

// Register a spawnable behavior
perform Otp.set_template(sup, "PoolWorker")

// Dynamically start children
let child1 = perform Otp.start_child(sup)
let child2 = perform Otp.start_child(sup)
```

## Linking and Monitoring

Actors can be linked or monitored without full supervision:

```nulang
// Link: abnormal exits propagate to the linked peer
perform Actor.link(target)

// Unlink: remove the link
perform Actor.unlink(target)

// Monitor: receive a DOWN message when target exits
perform Actor.monitor(target)

// Demonitor: stop monitoring
perform Actor.demonitor(target)
```

## Exit Trapping

When `trap_exit` is enabled, linked peer exits arrive as system messages instead of killing the actor. Exit signals are delivered as messages; the actor handles them in its normal message loop.

```nulang
perform Actor.trap_exit(true)
```

## Actor Exit Reasons

```nulang
perform Actor.exit(0)   // Normal
perform Actor.exit(1)   // Error
perform Actor.exit(2)   // Kill (cannot be trapped by trap_exit)
perform Actor.exit(99)  // Custom reason
```

Abnormal exits (non-zero) propagate to linked peers. Kills (`exit(2)`) bypass trap_exit and always cascade.

## Supervisor Exit Handling

When a supervisor receives an exit signal from a child:

| Exit Reason | Action |
|-------------|--------|
| `normal` | Child is removed, no restart |
| `kill` | Child is removed, no restart (already killed) |
| `error` or custom | Restart policy applied |

Supervisor actions cascade: if a supervisor restarts a child too many times in a short window, it escalates the failure to _its_ supervisor — all the way up the tree.

## Full Example

```nulang
actor PoolWorker {
    state id: Int = 0

    behavior init(worker_id: Int) {
        self.id = worker_id
    }

    behavior process(task: String) {
        // Simulate work — may fail
        if task == "crash" {
            perform Actor.exit(1)
        }
        perform IO.print("Worker " + perform Int.to_string(self.id) + " processed: " + task)
    }
}

// Main: Set up supervision tree
actor Main {
    behavior run() {
        let sup = perform Otp.create_supervisor("pool", 3)  // simple_one_for_one
        perform Otp.set_template(sup, "PoolWorker")

        let w1 = perform Otp.start_child(sup)
        let w2 = perform Otp.start_child(sup)

        send w1 process("hello")
        send w2 process("crash")  // Worker 2 exits with error → restarted
    }
}
```

## Actor Registry

Register actors by name for discovery:

```nulang
perform Actor.register("logger")
// ... elsewhere ...
let logger = perform Actor.whereis("logger")
```

Unregister when done:

```nulang
perform Actor.unregister("logger")
```

## Scheduling Priority

Control an actor's scheduling priority:

```nulang
perform Actor.set_priority(0)  // High
perform Actor.set_priority(1)  // Normal (default)
perform Actor.set_priority(2)  // Low
```

Priority affects scheduling order only — message order within a mailbox is always FIFO.

## Next

- [Actor Stdlib Reference](/stdlib/actor/) — full list of Actor.* built-in operations
- [OTP Stdlib Reference](/stdlib/otp/) — full list of Otp.* supervisor operations
- [Distribution & Clustering](/actors/distribution/) — run supervised actors across nodes
