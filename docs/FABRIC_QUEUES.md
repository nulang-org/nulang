# Nulang Fabric Queues and Compatibility Adapters

**Status:** Experimental
**Scope:** Native queue semantics first; BullMQ/NATS/JetStream adapters follow the native contract.

## Why this exists

Nulang actor mailboxes are actor-addressed protocol delivery. They are deliberately
not the generic durable queue abstraction.

Fabric Queues are named resources layered on Fabric Streams. They provide the
semantics needed by work queues and broker compatibility without creating a new
language-level execution primitive:

- durable enqueue,
- delayed visibility,
- priorities,
- worker leases / visibility timeouts,
- ACK and NACK,
- redelivery after lease expiry,
- bounded delivery attempts,
- durable completed/failed/dead-lettered state,
- stable job-id deduplication.

Fabric Queue now uses two append-only durable logs:

- `__queue.<name>` stores immutable job envelopes/payloads.
- `__queue_meta.<name>` stores queue configuration and every lease/state mutation.

`queue_state.json` is only a materialized cache. Each mutation is appended and
fsynced before that cache is replaced, and the cache stores the last applied
mutation sequence. If the cache is missing, Nulang rebuilds queue configuration,
jobs, deliveries, fencing tokens and terminal states from the two logs. If a
mutation is durable but the process crashes before cache replacement, recovery
replays it. Inconsistent cursors or payload indexes fail closed rather than
inventing state.

## Native API

The first runtime slice exposes:

- \`fabric_queue_create\`
- \`fabric_queue_add\`
- \`fabric_queue_acquire\`
- \`fabric_queue_ack\`
- \`fabric_queue_nack\`
- \`fabric_queue_renew\`
- \`fabric_queue_reap_expired\`
- \`fabric_queue_info\`

Deterministic \`*_at(..., now_ms)\` variants exist for tests and simulation.

Example:

\`\`\`rust
use nulang::runtime::{
    FabricQueueAddOptions, FabricQueueConfig, Runtime,
};

let mut runtime = Runtime::new();
runtime.fabric_stream_open("/var/lib/nulang/fabric")?;

runtime.fabric_queue_create(
    "emails",
    FabricQueueConfig {
        visibility_timeout_ms: 30_000,
        max_attempts: 5,
        dead_letter_queue: Some("emails-dlq".into()),
    },
)?;

let added = runtime.fabric_queue_add(
    "emails",
    "welcome",
    br#"{"userId":"123"}"#,
    FabricQueueAddOptions {
        job_id: Some("welcome:123".into()),
        priority: 10,
        delay_ms: 0,
    },
)?;

if let Some(job) = runtime.fabric_queue_acquire("emails", "worker-1")? {
    // Perform work using job.payload.
    runtime.fabric_queue_ack("emails", job.sequence, "worker-1", job.lease_token)?;
}
# Ok::<(), std::io::Error>(())
\`\`\`

## Delivery model

The native queue currently provides at-least-once delivery.

A delivery is a lease:

\`\`\`text
Waiting
  |
  | acquire
  v
Active (consumer, lease_until)
  |                 |
  | ACK             | lease expires / NACK
  v                 v
Completed        Waiting / terminal
\`\`\`

Every successful acquisition increments the delivery count. Once
\`max_attempts\` is reached, another expiry or NACK makes the job terminal:

- \`Failed\` when no DLQ is configured,
- \`DeadLettered\` when a DLQ name is configured.

The first slice records dead-letter terminal state but does not yet forward the
payload into the configured DLQ. That forwarding must be added atomically with
the terminal transition before the DLQ option is claimed as fully implemented.

Stable application job ids provide enqueue deduplication. This is not a claim
of exactly-once external side effects. Processors must still use idempotency
keys at external effect boundaries.

## Ordering

Ready jobs are selected by:

1. higher native priority first,
2. lower stream sequence first for equal priority.

Compatibility adapters are responsible for translating external priority
conventions into this native ordering.

## BullMQ v6 adapter

BullMQ 6 introduced the datastore-agnostic \`IQueueBackend\` contract. The
Nulang adapter should implement that interface directly instead of emulating
Redis.

Target package:

\`\`\`text
@nulang/bullmq-backend
\`\`\`

Usage target:

\`\`\`ts
import {
  Queue,
  Worker,
  setDefaultBackendFactory,
} from "bullmq";
import { createNulangBackend } from "@nulang/bullmq-backend";

setDefaultBackendFactory(
  createNulangBackend({
    endpoint: "https://us-east.nulang.cloud",
    token: process.env.NULANG_TOKEN!,
  }),
);

const queue = new Queue("emails");
await queue.add("welcome", { userId: "123" });

new Worker("emails", async job => {
  // Existing BullMQ processor code.
});
\`\`\`

### BullMQ semantic mapping

| BullMQ | Fabric Queue / Nulang |
| --- | --- |
| Queue | named Fabric Queue |
| Job | queue stream record + durable job state |
| Worker lock | visibility lease |
| extendLock | renew lease |
| stalled job | expired lease -> redelivery |
| delayed job | available_at timestamp / Time primitive |
| attempts | delivery count + retry policy |
| jobId | stable application id / dedup key |
| priority | native priority after adapter translation |
| QueueEvents | Fabric event stream |
| FlowProducer | durable workflow / dependency graph |
| Job Scheduler | durable schedule actor |
| rate limiting | queue policy / token bucket actor |
| failed terminal state | Failed |
| dead-letter policy | DLQ Fabric Queue |

### BullMQ implementation phases

The BullMQ adapter must not be published as production compatible until its
implementation passes BullMQ's adapter/conformance suite.

Phase B1 — core worker path:

- connection lifecycle,
- queue identity,
- addJob/addJobs,
- moveToActive,
- moveToCompleted,
- moveToFailed,
- moveToDelayed,
- retryJob,
- promote,
- extendLock,
- moveStalledJobsToWait,
- waitForJob,
- core getters needed by Queue/Worker/Job.

Phase B2 — administration and events:

- pause/resume,
- drain/clean/obliterate,
- QueueEvents,
- counts/ranges,
- progress/logs,
- bulk transitions.

Phase B3 — flows and schedules:

- addFlow atomicity,
- parent/child dependencies,
- waiting-children,
- Job Scheduler,
- repeat/schedule metadata.

The native queue engine should be extended where BullMQ exposes a generally
useful semantic. Redis-specific implementation details must stay in the
adapter, not leak into Fabric Queue.

## NATS compatibility

NATS compatibility should be split into Core NATS and JetStream.

### Core NATS

Core NATS maps to Fabric's ephemeral routing and actor messaging rather than to
Fabric Queue.

Target gateway:

\`\`\`text
nulang-nats-gateway :4222
\`\`\`

Initial protocol subset:

- INFO / CONNECT,
- PING / PONG,
- PUB / HPUB,
- SUB / UNSUB,
- subject wildcards,
- request/reply inboxes,
- queue groups,
- headers,
- no-responder behavior.

A successful Core NATS compatibility layer should let an application migrate by
changing only its NATS endpoint.

### JetStream

JetStream durability maps to Fabric Streams plus the Fabric Queue consumer
state machine:

| JetStream | Nulang |
| --- | --- |
| stream | Fabric Stream |
| durable consumer | durable queue/consumer state |
| ACK | queue ACK |
| NAK | queue NACK |
| AckWait | visibility lease |
| redelivery | lease expiry |
| MaxDeliver | max_attempts |
| Msg-Id | stable deduplication id |
| DLQ/advisory pattern | Fabric Queue / event stream |
| replica policy | Fabric Stream replication policy |
| committed sequence | Fabric committed sequence |

Do not implement JetStream wire/API compatibility until native consumer-group
ownership, retention, and replicated lease state are stable.

## Queue-level replicated ownership

A replicated queue cannot independently rendezvous-hash its payload and mutation
stream names. `__queue.<name>` and `__queue_meta.<name>` could otherwise select
different leaders, making a lease mutation and its job payload belong to
different consensus domains.

The replication layer therefore derives placement once from the logical key:

```text
__queue_owner.<name>
```

and installs that exact epoch, leader, membership fingerprint, replication
factor, and ordered replica set on both internal streams.

Bootstrap is fail-closed:

- both internal streams unowned + empty: derive and install one policy,
- both already carry the identical policy: reuse it,
- a crash leaves one side installed and the other unowned + empty: resume the
  same installed policy onto the missing side,
- divergent policies: reject,
- unowned durable history: reject and require explicit migration.

This bootstrap API remains crate-private until a queue-policy control message
installs the policy on followers before ordinary replicated queue traffic.
That follower propagation is required because ordinary per-stream first-contact
bootstrap would recompute placement from the internal stream name and defeat the
shared queue ownership invariant.

## Distributed follow-up

The current queue state machine is local to one Fabric stream store. Moving it
to replicated production semantics requires:

1. make queue mutable state a replicated/epoch-fenced state machine rather than
   a local JSON index,
2. assign consumer-group ownership under the installed stream epoch,
3. make lease acquisition a quorum-safe compare-and-set,
4. carry the existing monotonic lease fencing token through replicated consumer state,
5. replicate delayed/priority indexes or derive them deterministically,
6. atomically forward exhausted jobs to a DLQ,
7. propagate queue events through a committed Fabric event stream,
8. compact terminal job state without breaking deduplication windows,
9. add per-queue retention, max age/bytes/jobs, and deduplication-window
    policies.

## Safety invariants

The compatibility layers must preserve these invariants:

- a stream append is not a consumer ACK,
- a transport ACK is not a durable consumer ACK,
- an expired/stale lease holder cannot complete a newer delivery,
- a redelivery may repeat external effects unless the effect boundary
  deduplicates them,
- queue state never advances beyond a durable payload record,
- crash recovery may conservatively redeliver but must not invent completion,
- protocol compatibility cannot weaken Fabric's epoch fencing or committed
  stream boundary.

## Next implementation slice

1. Replicate both queue payload and mutation streams under one queue ownership
   policy, and expose only quorum-committed mutations to consumers.
2. Add replicated consumer-group ownership while preserving lease fencing tokens.
3. Make lease acquisition a quorum-safe compare-and-set against the installed
   queue/stream epoch.
4. Implement atomic DLQ forwarding.
5. Build the minimal BullMQ B1 backend against these APIs.
6. Run BullMQ's adapter conformance suite and use failures to drive only the
   missing generally useful native queue semantics.
7. Then build Core NATS wire compatibility; JetStream follows after replicated
   consumer semantics are proven.
