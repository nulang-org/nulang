import assert from 'node:assert/strict';
import { test } from 'node:test';

import { Queue, Worker, type JobJson } from 'bullmq';

import {
  NulangQueueBackend,
  NulangQueueSession,
  createNulangBackendFactory,
} from '../src/backend.js';
import {
  encodeBullMQJob,
  type NulangQueueAcquireRequest,
  type NulangQueueAddRequest,
  type NulangQueueAddResult,
  type NulangQueueClient,
  type NulangQueueCompleteRequest,
  type NulangQueueCounts,
  type NulangQueueDelivery,
  type NulangQueueFailRequest,
  type NulangQueueJob,
  type NulangQueueRenewRequest,
  type NulangQueueWaitSignal,
} from '../src/client.js';
import { NulangBullMQUnsupportedError } from '../src/errors.js';

function job(
  id: string,
  options: {
    priority?: number;
    delay?: number;
    attempts?: number;
  } = {},
): JobJson {
  return {
    id,
    name: 'render',
    data: JSON.stringify({ id }),
    opts: {
      attempts: options.attempts ?? 3,
      priority: options.priority,
      delay: options.delay,
    },
    progress: 0,
    attemptsMade: 0,
    attemptsStarted: 0,
    timestamp: 100,
    delay: options.delay ?? 0,
    priority: options.priority ?? 0,
    failedReason: '',
    returnvalue: 'null',
    stalledCounter: 0,
  };
}

class FakeClient implements NulangQueueClient {
  readonly adds: NulangQueueAddRequest[] = [];
  readonly acquires: NulangQueueAcquireRequest[] = [];
  readonly completes: NulangQueueCompleteRequest[] = [];
  readonly failures: NulangQueueFailRequest[] = [];
  readonly renewals: NulangQueueRenewRequest[] = [];
  readonly delayed: Array<{ queue: string; jobId: string; availableAtMs: number }> = [];
  readonly retried: Array<{ queue: string; jobId: string }> = [];
  readonly promoted: Array<{ queue: string; jobId: string }> = [];
  readonly waitTimeouts: number[] = [];
  readonly jobs = new Map<string, NulangQueueJob>();
  readonly meta = new Map<string, Record<string, string>>();

  deliveries: NulangQueueDelivery[] = [];
  reaped: string[] = [];
  waitSignal: NulangQueueWaitSignal | null = null;
  failAcquireCount = 0;
  closeCount = 0;
  disconnectCount = 0;
  blockingDisconnectCount = 0;
  blockingReconnectCount = 0;
  connectionName?: string;

  async waitUntilReady(): Promise<void> {}

  async close(_force?: boolean): Promise<void> {
    this.closeCount += 1;
  }

  async disconnect(): Promise<void> {
    this.disconnectCount += 1;
  }

  async setName(name: string): Promise<void> {
    this.connectionName = name;
  }

  async add(request: NulangQueueAddRequest): Promise<NulangQueueAddResult> {
    this.adds.push(request);
    return {
      jobId: request.jobId,
      sequence: this.adds.length,
      deduplicated: false,
      enqueued: true,
    };
  }

  async addMany(requests: NulangQueueAddRequest[]): Promise<NulangQueueAddResult[]> {
    return Promise.all(requests.map(request => this.add(request)));
  }

  async acquire(request: NulangQueueAcquireRequest): Promise<NulangQueueDelivery | null> {
    this.acquires.push(request);
    if (this.failAcquireCount > 0) {
      this.failAcquireCount -= 1;
      throw new Error('transient acquire error');
    }
    return this.deliveries.shift() ?? null;
  }

  async complete(request: NulangQueueCompleteRequest): Promise<void> {
    this.completes.push(request);
  }

  async fail(request: NulangQueueFailRequest): Promise<void> {
    this.failures.push(request);
  }

  async renew(request: NulangQueueRenewRequest): Promise<number> {
    this.renewals.push(request);
    return request.nowMs + request.extensionMs;
  }

  async delay(queue: string, jobId: string, availableAtMs: number): Promise<void> {
    this.delayed.push({ queue, jobId, availableAtMs });
  }

  async retry(queue: string, jobId: string): Promise<void> {
    this.retried.push({ queue, jobId });
  }

  async promote(queue: string, jobId: string): Promise<void> {
    this.promoted.push({ queue, jobId });
  }

  async reapExpired(_queue: string, _nowMs: number): Promise<string[]> {
    return this.reaped;
  }

  async getJob(queue: string, jobId: string): Promise<NulangQueueJob | undefined> {
    return this.jobs.get(`${queue}:${jobId}`);
  }

  async getState(queue: string, jobId: string) {
    return (await this.getJob(queue, jobId))?.state;
  }

  async getCounts(_queue: string): Promise<NulangQueueCounts> {
    return {
      waiting: 2,
      active: 1,
      completed: 3,
      failed: 4,
      delayed: 5,
      prioritized: 6,
      deadLettered: 2,
    };
  }

  async getCountsPerPriority(
    _queue: string,
    priorities: number[],
  ): Promise<number[]> {
    return priorities.map(priority => priority + 10);
  }

  async setQueueMeta(
    queue: string,
    values: Record<string, string | number>,
  ): Promise<number> {
    const current = this.meta.get(queue) ?? {};
    for (const [key, value] of Object.entries(values)) {
      current[key] = String(value);
    }
    this.meta.set(queue, current);
    return Object.keys(values).length;
  }

  async getQueueMeta(queue: string): Promise<Record<string, string>> {
    return { ...(this.meta.get(queue) ?? {}) };
  }

  async removeQueueMetaFields(queue: string, fields: string[]): Promise<number> {
    const current = this.meta.get(queue) ?? {};
    let removed = 0;
    for (const field of fields) {
      if (Object.hasOwn(current, field)) {
        delete current[field];
        removed += 1;
      }
    }
    this.meta.set(queue, current);
    return removed;
  }

  async waitForJob(
    _queue: string,
    blockTimeoutMs: number,
  ): Promise<NulangQueueWaitSignal | null> {
    this.waitTimeouts.push(blockTimeoutMs);
    return this.waitSignal;
  }

  async disconnectBlocking(): Promise<void> {
    this.blockingDisconnectCount += 1;
  }

  async reconnectBlocking(): Promise<void> {
    this.blockingReconnectCount += 1;
  }
}

function delivery(
  source: JobJson,
  overrides: Partial<NulangQueueDelivery> = {},
): NulangQueueDelivery {
  return {
    queue: 'paint',
    sequence: 1,
    jobId: source.id,
    name: source.name,
    payload: encodeBullMQJob(source),
    priority: 2_097_152,
    deliveries: 1,
    queueEpoch: 7,
    leaseToken: 11,
    leaseUntilMs: 30_100,
    ...overrides,
  };
}

function backend(client: FakeClient, now = 100): NulangQueueBackend {
  return new NulangQueueBackend(
    new NulangQueueSession(client),
    'paint',
    { connection: {} } as any,
    { now: () => now },
  );
}

test('translates BullMQ priority ordering and preserves stable job ids', async () => {
  const client = new FakeClient();
  const b = backend(client);

  const highest = job('job-0', { priority: 0 });
  const explicit = job('job-7', { priority: 7 });

  assert.equal(await b.addJob(highest, highest.id), 'job-0');
  assert.equal(await b.addJob(explicit, explicit.id), 'job-7');

  assert.equal(client.adds[0]?.jobId, 'job-0');
  assert.equal(client.adds[0]?.priority, 2_097_152);
  assert.equal(client.adds[1]?.priority, 2_097_145);
  assert.deepEqual(client.adds[1]?.payload, encodeBullMQJob(explicit));
});

test('reuses acquire operation id only until the claim resolves', async () => {
  const client = new FakeClient();
  const first = job('job-1');
  const second = job('job-2');
  client.failAcquireCount = 1;
  client.deliveries.push(delivery(first), delivery(second, {
    sequence: 2,
    jobId: 'job-2',
    leaseToken: 12,
  }));

  const b = backend(client);

  await assert.rejects(() => b.moveToActive('worker:1'));
  const firstOperation = client.acquires[0]?.operationId;
  assert.ok(firstOperation);

  const claimed = await b.moveToActive('worker:1', 'renderer');
  assert.equal(claimed[1], 'job-1');
  assert.equal(client.acquires[1]?.operationId, firstOperation);
  assert.equal(client.acquires[1]?.leaseDurationMs, 30_000);
  assert.equal((claimed[0] as JobJson).attemptsStarted, 1);
  assert.equal((claimed[0] as JobJson).processedBy, 'renderer');

  const completed = await b.moveToCompleted(
    { id: 'job-1' } as any,
    { ok: true },
    false,
    'worker:1',
    true,
  );

  assert.equal(client.completes.length, 1);
  assert.equal(client.completes[0]?.queueEpoch, 7);
  assert.equal(client.completes[0]?.leaseToken, 11);
  assert.equal(client.completes[0]?.consumer, 'worker:1');
  assert.equal((completed.result as any[])[1], 'job-2');
  assert.notEqual(client.acquires[2]?.operationId, firstOperation);
});

test('maps BullMQ lock renewal to Fabric epoch and lease fencing', async () => {
  const client = new FakeClient();
  const source = job('job-lock');
  client.deliveries.push(delivery(source));
  const b = backend(client, 500);

  await b.moveToActive('worker:lock');

  assert.equal(await b.extendLock('job-lock', 'wrong', 10_000), 0);
  assert.equal(client.renewals.length, 0);

  assert.equal(await b.extendLock('job-lock', 'worker:lock', 10_000), 1);
  assert.equal(client.renewals.length, 1);
  assert.equal(client.renewals[0]?.queueEpoch, 7);
  assert.equal(client.renewals[0]?.leaseToken, 11);
  assert.equal(client.renewals[0]?.extensionMs, 10_000);
  assert.equal(client.renewals[0]?.nowMs, 500);
});

test('routes immediate and delayed retries through non-terminal fenced failure', async () => {
  const client = new FakeClient();
  const first = job('job-retry');
  const second = job('job-delay');
  client.deliveries.push(
    delivery(first),
    delivery(second, { sequence: 2, jobId: 'job-delay', leaseToken: 12 }),
  );
  const b = backend(client, 1_000);

  await b.moveToActive('worker:retry');
  await b.retryJob('job-retry', false, 'worker:retry', {
    fieldsToUpdate: { failedReason: 'retry me' },
  });
  assert.equal(client.failures[0]?.terminal, false);
  assert.equal(client.failures[0]?.delayMs, 0);
  assert.equal(client.failures[0]?.failedReason, 'retry me');

  await b.moveToActive('worker:delay');
  await b.moveToDelayed('job-delay', 1_000, 750, 'worker:delay', {
    fieldsToUpdate: { failedReason: 'backoff' },
  });
  assert.equal(client.failures[1]?.terminal, false);
  assert.equal(client.failures[1]?.delayMs, 750);
  assert.equal(client.failures[1]?.failedReason, 'backoff');
});

test('moveToFailed is terminal and preserves lease fencing', async () => {
  const client = new FakeClient();
  const source = job('job-fail');
  client.deliveries.push(delivery(source));
  const b = backend(client, 2_000);

  await b.moveToActive('worker:fail');
  const result = await b.moveToFailed(
    { id: 'job-fail' } as any,
    'boom',
    false,
    'worker:fail',
    false,
    { stacktrace: 'stack' },
  );

  assert.equal(result.finishedOn, 2_000);
  assert.equal(client.failures[0]?.terminal, true);
  assert.equal(client.failures[0]?.failedReason, 'boom');
  assert.equal(client.failures[0]?.fieldsToUpdate?.stacktrace, 'stack');
  assert.equal(client.failures[0]?.queueEpoch, 7);
  assert.equal(client.failures[0]?.leaseToken, 11);
});

test('maps stalled recovery and blocking wait to native queue primitives', async () => {
  const client = new FakeClient();
  client.reaped = ['a', 'b'];
  client.waitSignal = { member: 'paint', score: 1234 };
  const b = backend(client);

  assert.deepEqual(await b.moveStalledJobsToWait(), ['a', 'b']);
  assert.deepEqual(await b.waitForJob(2.5), {
    member: 'paint',
    score: 1234,
  });
  assert.equal(client.waitTimeouts[0], 2_500);

  await b.disconnectBlocking();
  await b.reconnectBlocking();
  assert.equal(client.blockingDisconnectCount, 1);
  assert.equal(client.blockingReconnectCount, 1);
});

test('supports core getters and minimal BullMQ queue metadata only', async () => {
  const client = new FakeClient();
  const source = job('job-state', { priority: 4 });
  client.jobs.set('paint:job-state', {
    jobId: 'job-state',
    sequence: 1,
    name: 'render',
    payload: encodeBullMQJob(source),
    priority: 2_097_148,
    state: 'waiting',
    deliveries: 0,
    availableAtMs: 50,
  });
  const b = backend(client, 100);

  assert.equal(await b.getState('job-state'), 'prioritized');
  assert.deepEqual(await b.getCounts(['waiting', 'active', 'failed']), [
    2,
    1,
    6,
  ]);
  assert.deepEqual(await b.getCountsPerPriority([0, 4]), [10, 14]);

  assert.equal(
    await b.setQueueMeta({ version: 'bullmq:6.3.8', 'opts.maxLenEvents': 1000 }),
    2,
  );
  assert.equal(await b.getQueueMetaField('version'), 'bullmq:6.3.8');
  await assert.rejects(
    () => b.setQueueMeta({ concurrency: 2 }),
    (error: unknown) =>
      error instanceof NulangBullMQUnsupportedError &&
      error.code === 'ERR_NULANG_BULLMQ_UNSUPPORTED',
  );
});

test('uses datastore-neutral identity and explicitly rejects B2/B3 operations', async () => {
  const client = new FakeClient();
  const b = backend(client);

  assert.equal(b.qualifiedName, 'paint');
  assert.equal(b.toKey('job-1'), 'paint:job-1');
  assert.deepEqual(b.parseNodeKey('paint:job-1'), {
    prefix: '',
    queueName: 'paint',
    id: 'job-1',
  });
  assert.match(b.clientName(':worker'), /^nulang:/);

  await assert.rejects(
    () => b.addFlow([]),
    (error: unknown) =>
      error instanceof NulangBullMQUnsupportedError &&
      error.operation === 'addFlow',
  );
});

test('shares a client session and closes it only after the last backend closes', async () => {
  const client = new FakeClient();
  const session = new NulangQueueSession(client);
  const first = new NulangQueueBackend(
    session,
    'one',
    { connection: {} } as any,
  );
  const second = new NulangQueueBackend(
    session,
    'two',
    { connection: {} } as any,
  );

  await first.close();
  assert.equal(client.closeCount, 0);
  await second.close();
  assert.equal(client.closeCount, 1);
});


test('runs the real BullMQ Queue -> Worker -> Job completion path without Redis', async () => {
  const client = new FakeClient();
  const factory = createNulangBackendFactory(client, { now: () => 1_000 });

  const queue = new Queue(
    'paint',
    { connection: {} } as any,
    factory,
  );
  await queue.waitUntilReady();

  const queued = await queue.add(
    'render',
    { asset: 'hero' },
    { jobId: 'integration-1', attempts: 2, priority: 5 },
  );
  assert.equal(queued.id, 'integration-1');
  assert.equal(client.adds.length, 1);
  assert.equal(client.adds[0]?.jobId, 'integration-1');

  const native = client.adds[0];
  assert.ok(native);
  client.deliveries.push({
    queue: 'paint',
    sequence: 1,
    jobId: 'integration-1',
    name: 'render',
    payload: native.payload,
    priority: native.priority,
    deliveries: 1,
    queueEpoch: 9,
    leaseToken: 21,
    leaseUntilMs: 31_000,
  });

  const worker = new Worker(
    'paint',
    null,
    {
      connection: {},
      autorun: false,
      lockDuration: 30_000,
    } as any,
    factory,
  );
  await worker.waitUntilReady();

  const active = await worker.getNextJob('worker:integration', { block: false });
  assert.ok(active);
  assert.equal(active.id, 'integration-1');
  assert.deepEqual(active.data, { asset: 'hero' });
  assert.equal(active.attemptsStarted, 1);

  await active.moveToCompleted(
    { rendered: true },
    'worker:integration',
    false,
  );

  assert.equal(client.completes.length, 1);
  assert.equal(client.completes[0]?.queueEpoch, 9);
  assert.equal(client.completes[0]?.leaseToken, 21);
  assert.deepEqual(client.completes[0]?.returnValue, { rendered: true });

  await worker.close();
  await queue.close();
  assert.equal(client.closeCount, 1);
});
