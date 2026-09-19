# @nulang/bullmq-backend

Experimental BullMQ v6 `IQueueBackend` adapter for Nulang Fabric Queue.

## B1 scope

This package maps BullMQ's core Queue/Worker path onto native Fabric Queue
semantics. It does **not** emulate Redis commands.

Implemented and validated against BullMQ 6.3.8:

- backend lifecycle and datastore-neutral queue identity
- `addJob` / `addJobs`
- per-job `attempts`
- BullMQ priority translation
- `moveToActive` with per-worker `lockDuration`
- `moveToCompleted` with durable return values
- terminal `moveToFailed`
- active-job delayed and immediate retries
- lock extension backed by Fabric `queue_epoch + lease_token` fencing
- stalled lease recovery
- `waitForJob` through the committed queue readiness signal
- core job/state/count getters
- minimal BullMQ queue metadata
- a concrete JSON-over-HTTP `NulangHttpQueueClient`

Operations outside the implemented surface throw
`ERR_NULANG_BULLMQ_UNSUPPORTED`; they never silently no-op.

## Live node transport

Build/run a Nulang node with the experimental loopback queue API:

```sh
nulang node \
  --listen 127.0.0.1:9000 \
  --plaintext \
  --queue-api 127.0.0.1:9091 \
  --queue-store .nulang/fabric
```

The queue API is intentionally **loopback-only** in this slice. The CLI rejects
non-loopback binds until an authenticated/TLS-protected public API is designed.

Create the BullMQ backend:

```ts
import { Queue, Worker } from 'bullmq';
import {
  NulangHttpQueueClient,
  createNulangBackendFactory,
} from '@nulang/bullmq-backend';

const client = new NulangHttpQueueClient({
  endpoint: 'http://127.0.0.1:9091',
  replicationFactor: 1,
});

const backendFactory = createNulangBackendFactory(client);

const queue = new Queue('render', { connection: {} }, backendFactory);
await queue.add(
  'render-image',
  { asset: 'hero' },
  { jobId: 'render:hero', attempts: 3 },
);

const worker = new Worker(
  'render',
  async job => render(job.data),
  { connection: {}, lockDuration: 30_000 },
  backendFactory,
);
```

The same adapter can use another implementation of `NulangQueueClient`; the
BullMQ compatibility layer is intentionally independent from its transport.

## Semantics

BullMQ job IDs become stable Fabric Queue deduplication IDs. BullMQ worker lock
tokens are bound to Fabric deliveries carrying a queue ownership epoch and a
monotonic lease token. Completion, retry, failure, and renewal therefore require
the exact fencing values belonging to the active delivery.

BullMQ `attempts` is stored as a per-job Fabric Queue attempt cap rather than
being approximated with a queue-wide setting. BullMQ `lockDuration` is also
carried per acquisition and is persisted in the replicated lease mutation.

BullMQ and Fabric use opposite priority directions. The adapter translates
BullMQ priority 0 to the highest native priority and preserves the ordering of
subsequent priorities.

Completion return values are stored in the committed Fabric Queue mutation log,
so a later BullMQ `getJob` can recover `returnvalue` after restart.

Delivery remains at-least-once. Stable job/operation IDs provide retry-safe
state transitions; this package does not claim exactly-once external side
effects.

## Current limits

The core producer/worker path works through a live Nulang node, but B1 is still
experimental. The HTTP client does not yet implement non-active
`changeDelay`/retry/promote operations, and aggregate getters do not yet split
all waiting jobs into BullMQ's delayed/prioritized categories.

Flows, parent/child dependencies, schedulers, QueueEvents, progress/logs,
administrative cleanup, rate limiting, and other B2/B3 operations remain
explicitly unsupported.

The package must pass BullMQ's broader backend/full-suite conformance before it
is described as production compatible.
