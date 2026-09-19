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
        max_attempts: Some(5),
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

For replicated queues, exhausted jobs use a crash-safe target-first DLQ handoff:
the destination payload must reach quorum before the source may commit its
`DeadLettered` transition. The intermediate state “target durable + source
still Active” is retryable; “source terminal + target missing” is not produced
by the replicated path.

A job may override the queue-wide `max_attempts`. The override is part of the
immutable job envelope and therefore also part of stable job-id dedup fencing.

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

Current live transport:

```sh
nulang node \
  --listen 127.0.0.1:9000 \
  --plaintext \
  --queue-api 127.0.0.1:9091 \
  --queue-store .nulang/fabric
```

```ts
import { Queue, Worker } from "bullmq";
import {
  NulangHttpQueueClient,
  createNulangBackendFactory,
} from "@nulang/bullmq-backend";

const client = new NulangHttpQueueClient({
  endpoint: "http://127.0.0.1:9091",
  replicationFactor: 1,
});
const backendFactory = createNulangBackendFactory(client);

const queue = new Queue("emails", { connection: {} }, backendFactory);
await queue.add("welcome", { userId: "123" }, {
  jobId: "welcome:123",
  attempts: 3,
});

new Worker(
  "emails",
  async job => sendWelcome(job.data),
  { connection: {}, lockDuration: 30_000 },
  backendFactory,
);
```

The experimental HTTP gateway is loopback-only. A node refuses a non-loopback
`--queue-api` bind until authentication/TLS is added.

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

Queue policy installation now propagates through reserved NUL0 system actor
messages:

- `__nulang_fabric_queue_policy_v1`
- `__nulang_fabric_queue_policy_ack_v1`

The leader persists the shared policy locally, sends that exact policy to every
configured follower, and tracks application-level installation acknowledgements
separately from transport ACKs. RF=1 is immediately ready; RF>1 remains gated
until every configured replica has acknowledged the same queue, epoch,
membership fingerprint, replication factor, leader and replica set.

Followers install the exact policy onto both internal streams before replying.
Conflicting policies, duplicate replicas, unauthorized senders, unknown
replicas, partial-history repair and stale ACKs fail closed.

The legacy local `fabric_queue_*` mutation/read APIs also reject any queue that
already carries a replication policy. This prevents callers from accidentally
bypassing the committed replicated path while that path is being completed.

Replicated queue creation is exposed as `fabric_queue_create_replicated`.
Creation is a resumable operation:

1. synchronize the queue policy,
2. do not append queue metadata until policy synchronization is ready,
3. append `QueueCreated` as mutation sequence 1 through
   `fabric_stream_replicated_append`,
4. on retry/restart, inspect sequence 1 and reconstruct/retry its durable
   replication ticket rather than appending another creation event,
5. report `created = true` only after sequence 1 is quorum committed.

A deterministic three-node RF=3 test exercises the full policy install/ACK,
retry, stream replica ACK, quorum commit and committed-boundary propagation
path.

Replicated queue inspection now reconstructs a transient state machine strictly
from `read_committed` on both internal streams. It never reads
`queue_state.json`, never decodes an uncommitted tail, and never performs
local lease expiry. A regression test appends malformed uncommitted records to
both logs and confirms they remain invisible to replicated queue info.

Replicated enqueue is exposed through `fabric_queue_add_replicated`:

- `QueueCreated` must already be quorum committed,
- the immutable job envelope is replicated through the queue payload stream,
- the job becomes visible only when that payload sequence is committed,
- a stable `job_id` makes retries resume the same durable sequence and
  replication intent,
- duplicate durable records for one `job_id` fail closed,
- without a stable `job_id`, each API invocation is intentionally a distinct
  enqueue; compatibility adapters should generate stable operation IDs for
  transport retries.

Replicated worker acquisition is exposed through
`fabric_queue_acquire_replicated`, with
`fabric_queue_acquire_replicated_with_lease_duration` for clients such as
BullMQ that carry a per-worker visibility timeout. It is leader-serialized and
quorum-gated:

- candidate selection uses only the committed queue state,
- priority ordering remains highest-priority first and FIFO within a priority,
- every replicated lease carries the installed queue epoch and a monotonic
  per-job lease token,
- callers supply a stable acquire `operation_id`,
- the first call appends one `LeaseAcquired` metadata mutation,
- retries with the same operation id resume that exact mutation,
- a different acquire is rejected with `WouldBlock` while metadata has an
  uncommitted tail,
- no `FabricQueueDelivery` is returned until the lease mutation quorum
  commits,
- replicated deliveries expose both `queue_epoch` and `lease_token` so
  future ACK/NACK/renew operations can reject stale workers after either a
  lease turnover or queue ownership epoch change.

Expired active leases are transitioned by
`fabric_queue_reap_expired_replicated`. Expiry is itself a replicated
metadata mutation: the job remains Active until `LeaseExpired` reaches
quorum, then becomes Waiting or terminal according to max-attempts/DLQ policy.
A second worker therefore cannot receive an expired job until the expiry
transition is committed. The reaper processes one deterministic due lease at a
time and resumes an uncommitted expiry rather than appending a competing
mutation.

Replicated worker completion and lease maintenance are also quorum-gated:

- `fabric_queue_ack_replicated` appends a `Completed` mutation,
- `fabric_queue_ack_replicated_with_result` durably records processor result
  bytes and fences retry reuse against a different result,
- `fabric_queue_nack_replicated` appends a `Nacked` mutation and only
  exposes the resulting Waiting/Failed/DeadLettered state after commit,
- `fabric_queue_renew_replicated` appends a `LeaseRenewed` mutation and
  only exposes the renewed deadline after commit,
- every operation requires the exact `queue_epoch + lease_token` from the
  delivery plus a stable operation id,
- retrying the same operation id resumes the same durable metadata sequence,
- reusing an operation id for a different mutation, job, consumer, epoch or
  lease token fails closed,
- stale queue epochs and stale lease tokens are rejected before a new metadata
  mutation is appended,
- while any metadata mutation is uncommitted, a different worker mutation
  returns `WouldBlock` rather than racing committed queue state.

This keeps delivery completion, retry scheduling, and lease extension in the
same serialized quorum state machine as acquisition.

Replicated worker concurrency domains are configured with
`fabric_queue_configure_consumer_group_replicated` and consumed through
`fabric_queue_acquire_consumer_group_replicated`.

These groups are **not** fan-out subscriptions. They are durable worker-pool
concurrency domains over one work queue:

- configuration is journaled in the queue metadata stream,
- `name + max_concurrency` is immutable in this slice,
- retries resume the original configuration mutation,
- conflicting reconfiguration fails closed,
- grouped leases persist the group name with the lease,
- replay rejects a committed mutation prefix that would exceed the configured
  concurrency cap,
- acquisition counts only quorum-committed Active leases in that group,
- if `active >= max_concurrency`, acquire returns no delivery and appends no
  lease mutation,
- capacity reopens only after the prior grouped lease leaves Active through a
  committed ACK, NACK, or LeaseExpired mutation,
- `fabric_queue_consumer_group_info_replicated` reports committed
  `active/max_concurrency` state.

Ungrouped workers continue to use `fabric_queue_acquire_replicated`; grouped
and ungrouped workers still compete for the same underlying jobs.

Replicated dead-letter forwarding uses a crash-safe target-first handoff rather
than claiming a cross-queue transaction:

- the source queue's configured DLQ target must be explicitly prepared with
  `fabric_queue_prepare_dead_letter_target_replicated`,
- that target is installed with the source queue's exact epoch, leader, replica
  set, partition, and membership fingerprint,
- exhausted NACK and lease-expiry paths refuse to mark the source
  `DeadLettered` unless the handoff path is used,
- the destination payload is replicated first using deterministic job id
  `__dlq:<source-queue>:<source-sequence>`,
- stable job-id retries validate immutable name, payload, priority, and delay;
  conflicting reuse fails closed,
- only after the destination payload is quorum committed may the source append
  its `DeadLettered` mutation,
- a crash after destination commit but before source terminalization leaves the
  source Active and the destination durable; retry deduplicates the destination
  and resumes source terminalization,
- terminal NACK and terminal lease expiry both use this same protocol.

This yields lossless, retry-safe handoff without describing two independently
replicated queues as one atomic storage transaction.

Dead-letter forwarding is implemented as a crash-safe target-first handoff.

Because Fabric Stream writes are leader-local, the source queue and its DLQ
target must share one queue ownership policy. Prepare the target with
`fabric_queue_prepare_dead_letter_target_replicated`, which installs the
source queue's exact epoch/leader/replica placement on an empty target before
QueueCreated is replicated there.

Terminal NACK and terminal lease expiry then follow this protocol:

1. derive a deterministic target job id: `__dlq:<source>:<sequence>`,
2. enqueue the original immutable job into the DLQ target,
3. wait until that target payload sequence reaches quorum,
4. only then append `DeadLettered` to the source queue metadata stream,
5. wait for the source terminal mutation to reach quorum.

A crash after step 3 cannot lose the job: the source remains Active while the
DLQ copy is already durable. Retrying the handoff reuses the deterministic job
id and resumes/deduplicates the target append, then resumes source
terminalization. Stable job-id dedup also validates immutable name, payload,
priority, and delay so a conflicting record cannot hijack the retry identity.

This is deliberately described as a crash-safe handoff rather than a
cross-queue ACID transaction. The observable intermediate state
`target durable + source still Active` is safe and retryable; the unsafe state
`source terminal + target missing` is never produced by the replicated DLQ
path.

Generic replicated NACK/expiry fail closed instead of directly marking a
DLQ-configured job DeadLettered without forwarding.


## Remaining distributed / compatibility follow-up

The core replicated queue state machine now covers queue-level ownership,
policy synchronization, quorum creation/enqueue, epoch-fenced acquisition,
ACK/NACK/renew, lease expiry/redelivery, consumer-group concurrency, per-job
attempt caps, per-acquire lease duration, durable completion results, committed
job/readiness views, and crash-safe DLQ forwarding.

Remaining work is primarily compatibility and production hardening:

1. add authenticated/TLS-protected remote queue API access and leader
   discovery/redirect instead of the current loopback leader endpoint,
2. add native replicated mutations for non-active promote/change-delay/manual
   retry so the BullMQ HTTP client can implement those without approximation,
3. expose richer committed indexes/counts for BullMQ delayed/prioritized/range
   getters,
4. add a committed queue event stream and QueueEvents mapping,
5. add retention/compaction policies without breaking dedup windows,
6. complete BullMQ B2 administration/events and B3 flows/schedulers,
7. run the broader BullMQ backend/full-suite conformance matrix,
8. implement Core NATS wire compatibility, then JetStream compatibility.

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

1. Finish the remaining BullMQ B1 non-active mutations and richer getters.
2. Run BullMQ's broader backend/full-suite conformance and use failures to drive
   only generally useful native queue semantics.
3. Add authenticated remote/leader-aware queue transport for Nulang Cloud.
4. Then build Core NATS wire compatibility; JetStream follows the durable
   consumer semantics already proven by Fabric Queue.
