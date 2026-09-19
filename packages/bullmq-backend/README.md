# @nulang/bullmq-backend

Experimental BullMQ v6 `IQueueBackend` adapter for Nulang Fabric Queue.

## B1 scope

This package maps BullMQ's core Queue/Worker path onto native Fabric Queue
semantics. It does **not** emulate Redis commands.

Implemented in B1:

- backend lifecycle and logical queue identity
- `addJob` / `addJobs`
- `moveToActive`
- `moveToCompleted` / terminal `moveToFailed`
- delayed and immediate retry transitions
- `promote`
- lock extension backed by Fabric `queue_epoch + lease_token` fencing
- stalled lease recovery
- `waitForJob`
- core job/state/count getters
- minimal queue metadata used by BullMQ itself

BullMQ operations outside B1 throw
`ERR_NULANG_BULLMQ_UNSUPPORTED`; they never silently no-op.

## Semantics

BullMQ job IDs are Nulang stable deduplication IDs. BullMQ worker lock tokens
are bound to Fabric deliveries carrying a queue epoch and monotonic lease token.
A completion/failure/renew call must present the BullMQ token that owns that
delivery; the adapter then submits the corresponding Fabric fencing values.

BullMQ and Nulang order priority in opposite directions. The adapter translates
BullMQ priorities at the boundary: priority 0 remains highest, then 1, 2, and so
on, while Fabric internally receives a descending integer priority.

Delivery is at-least-once. Stable job/operation IDs provide effectively-once
state transitions when callers retry; this package does not claim exactly-once
external side effects.

## Creating a backend factory

```ts
import { Queue, Worker } from 'bullmq';
import { createNulangBackendFactory } from '@nulang/bullmq-backend';

const backendFactory = createNulangBackendFactory(nulangQueueClient);

const queue = new Queue('render', { connection: {} }, backendFactory);
const worker = new Worker(
  'render',
  async job => render(job.data),
  { connection: {}, backendFactory },
);
```

The concrete `NulangQueueClient` transport is intentionally separate from the
BullMQ adapter. Nulang Cloud can implement it over its API while embedded users
can bind it directly to the runtime.

## Status

B1 is experimental. Passing this package's contract tests is not sufficient to
call the backend production-ready; BullMQ backend/full-suite conformance must
also pass.
