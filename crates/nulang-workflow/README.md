# nulang-workflow

Experimental Cloud SDK substrate for durable workflows.

This crate intentionally has **zero dependency on the Nulang compiler/runtime
crate**. It follows the same boundary as `nulang-ai`: the SDK owns portable
workflow semantics and hosts implement a small runtime trait.

## Current contract

The first slice implements durable activity execution:

- stable logical invocation identity derived from workflow + step + occurrence +
  operation;
- prepare-before-dispatch history records;
- replay validation that fails closed if an in-flight activity changes;
- replay of recorded completions without redispatch;
- optimistic history revisions so multiple orchestrators cannot silently race;
- retry policy pinned into history, including durable retry deadlines;
- one idempotency key reused across retries and crash recovery;
- durable worker leases with heartbeat renewal and monotonic fencing tokens;
- plan-pinned sagas with reverse-order, crash-resumable compensation;
- idempotent signal notification/delivery and persisted, re-armable timers.

It deliberately does **not** claim arbitrary exactly-once delivery. If an
external system commits and the host crashes before recording completion, the
activity may be redispatched with the same idempotency key. Effective once-only
behavior therefore still requires cooperation from the external dependency.

## Runtime integration

A local VM adapter and Nulang Cloud adapter should implement
`WorkflowRuntime` using the same semantics:

1. load a workflow history and revision;
2. append an event with compare-and-set revision semantics;
3. dispatch an activity while forwarding the supplied idempotency key;
4. provide the host clock used only to materialize durable retry deadlines.

The next SDK slices are a Nulang Cloud workflow adapter, worker-pool
distribution, and higher-level workflow composition.
