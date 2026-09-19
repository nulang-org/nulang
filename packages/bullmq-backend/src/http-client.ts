import { createHash } from 'node:crypto';
import type {
  NulangQueueAcquireRequest,
  NulangQueueAddRequest,
  NulangQueueAddResult,
  NulangQueueClient,
  NulangQueueCompleteRequest,
  NulangQueueCounts,
  NulangQueueDelivery,
  NulangQueueFailRequest,
  NulangQueueJob,
  NulangQueueRenewRequest,
  NulangQueueRequeueRequest,
  NulangQueueState,
  NulangQueueWaitSignal,
} from './client.js';

export interface NulangHttpQueueClientOptions {
  endpoint: string;
  partition?: number;
  replicationFactor?: number;
  visibilityTimeoutMs?: number;
  queueMaxAttempts?: number;
  pollIntervalMs?: number;
  operationTimeoutMs?: number;
}

interface ApiEnvelope<T> {
  ok: boolean;
  result?: T;
  error?: {
    kind: string;
    message: string;
  };
}

export class NulangQueueApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly kind: string,
    message: string,
  ) {
    super(message);
    this.name = 'NulangQueueApiError';
  }
}

export class NulangHttpQueueClient implements NulangQueueClient {
  private readonly endpoint: string;
  private readonly partition: number;
  private readonly replicationFactor: number;
  private readonly visibilityTimeoutMs: number;
  private readonly queueMaxAttempts: number;
  private readonly pollIntervalMs: number;
  private readonly operationTimeoutMs: number;
  private readonly metadata = new Map<string, Record<string, string>>();
  private readonly ensuredQueues = new Set<string>();

  private closed = false;
  private blockingGeneration = 0;
  private connectionName?: string;

  constructor(options: NulangHttpQueueClientOptions) {
    this.endpoint = options.endpoint.replace(/\/$/, '');
    this.partition = options.partition ?? 0;
    this.replicationFactor = options.replicationFactor ?? 1;
    this.visibilityTimeoutMs = options.visibilityTimeoutMs ?? 30_000;
    this.queueMaxAttempts = options.queueMaxAttempts ?? 1;
    this.pollIntervalMs = options.pollIntervalMs ?? 25;
    this.operationTimeoutMs = options.operationTimeoutMs ?? 30_000;
  }

  async waitUntilReady(): Promise<void> {
    this.assertOpen();
    // The first queue operation is the readiness probe. There is deliberately
    // no process-global health dependency in the queue protocol.
  }

  async close(_force?: boolean): Promise<void> {
    this.closed = true;
    this.blockingGeneration += 1;
  }

  async disconnect(): Promise<void> {
    this.closed = true;
    this.blockingGeneration += 1;
  }

  async setName(name: string): Promise<void> {
    this.connectionName = name;
  }

  async add(request: NulangQueueAddRequest): Promise<NulangQueueAddResult> {
    await this.ensureQueue(request.queue);
    const result = await this.retryUntil(
      () =>
        this.call<{
          sequence: number | null;
          deduplicated: boolean;
          enqueued: boolean;
          committed: boolean;
        }>({
          op: 'add',
          queue: request.queue,
          name: request.name,
          payload_hex: toHex(request.payload),
          job_id: request.jobId,
          priority: request.priority,
          delay_ms: request.delayMs,
          max_attempts: request.maxAttempts,
          partition: this.partition,
          replication_factor: this.replicationFactor,
          now_ms: request.timestampMs,
        }),
      value => value.enqueued && value.sequence != null,
      `enqueue ${request.queue}/${request.jobId}`,
    );

    return {
      jobId: request.jobId,
      sequence: result.sequence!,
      deduplicated: result.deduplicated,
      enqueued: result.enqueued,
    };
  }

  async acquire(
    request: NulangQueueAcquireRequest,
  ): Promise<NulangQueueDelivery | null> {
    await this.ensureQueue(request.queue);
    const deadline = Date.now() + this.operationTimeoutMs;

    for (;;) {
      const result = await this.call<{
        mutationSequence: number | null;
        committed: boolean;
        delivery: null | {
          sequence: number;
          queueEpoch: number;
          jobId: string;
          name: string;
          payloadHex: string;
          priority: number;
          deliveries: number;
          leaseToken: number;
          leaseUntilMs: number;
        };
        resumed: boolean;
      }>({
        op: 'acquire',
        queue: request.queue,
        consumer: request.consumer,
        operation_id: request.operationId,
        lease_duration_ms: request.leaseDurationMs,
        partition: this.partition,
        replication_factor: this.replicationFactor,
        now_ms: request.nowMs,
      });

      if (result.delivery) {
        return {
          queue: request.queue,
          sequence: result.delivery.sequence,
          jobId: result.delivery.jobId,
          name: result.delivery.name,
          payload: fromHex(result.delivery.payloadHex),
          priority: result.delivery.priority,
          deliveries: result.delivery.deliveries,
          queueEpoch: result.delivery.queueEpoch,
          leaseToken: result.delivery.leaseToken,
          leaseUntilMs: result.delivery.leaseUntilMs,
        };
      }
      if (result.mutationSequence == null) {
        return null;
      }
      await this.sleepUntilRetry(deadline, `acquire ${request.queue}`);
    }
  }

  async complete(request: NulangQueueCompleteRequest): Promise<void> {
    await this.ensureQueue(request.queue);
    const encoded = new TextEncoder().encode(
      JSON.stringify(request.returnValue ?? null),
    );
    await this.retryUntil(
      () =>
        this.call<{
          completed: boolean;
          committed: boolean;
        }>({
          op: 'ack',
          queue: request.queue,
          sequence: request.sequence,
          consumer: request.consumer,
          queue_epoch: request.queueEpoch,
          lease_token: request.leaseToken,
          operation_id: request.operationId,
          result_hex: toHex(encoded),
          partition: this.partition,
          replication_factor: this.replicationFactor,
          now_ms: request.nowMs,
        }),
      value => value.completed && value.committed,
      `complete ${request.queue}/${request.jobId}`,
    );
  }

  async fail(request: NulangQueueFailRequest): Promise<void> {
    await this.ensureQueue(request.queue);
    const result = await this.retryUntil(
      () =>
        this.call<{
          mutationSequence: number | null;
          committed: boolean;
          transition: null | {
            status: NulangQueueState;
            deliveries: number;
            availableAtMs: number | null;
          };
        }>({
          op: 'nack',
          queue: request.queue,
          sequence: request.sequence,
          consumer: request.consumer,
          queue_epoch: request.queueEpoch,
          lease_token: request.leaseToken,
          operation_id: request.operationId,
          delay_ms: request.delayMs,
          error: request.failedReason,
          partition: this.partition,
          replication_factor: this.replicationFactor,
          now_ms: request.nowMs,
        }),
      value => value.committed && value.transition != null,
      `fail/retry ${request.queue}/${request.jobId}`,
    );

    const status = result.transition!.status;
    if (request.terminal && status === 'waiting') {
      throw new Error(
        `Nulang queue kept terminal BullMQ failure ${request.jobId} waiting`,
      );
    }
  }

  async renew(request: NulangQueueRenewRequest): Promise<number> {
    await this.ensureQueue(request.queue);
    const result = await this.retryUntil(
      () =>
        this.call<{
          committed: boolean;
          leaseUntilMs: number | null;
        }>({
          op: 'renew',
          queue: request.queue,
          sequence: request.sequence,
          consumer: request.consumer,
          queue_epoch: request.queueEpoch,
          lease_token: request.leaseToken,
          operation_id: request.operationId,
          extension_ms: request.extensionMs,
          partition: this.partition,
          replication_factor: this.replicationFactor,
          now_ms: request.nowMs,
        }),
      value => value.committed && value.leaseUntilMs != null,
      `renew ${request.queue}/${request.jobId}`,
    );
    return result.leaseUntilMs!;
  }

  async requeue(request: NulangQueueRequeueRequest): Promise<void> {
    await this.ensureQueue(request.queue);
    const operationId = `bull:requeue:${createHash('sha256')
      .update(
        [
          request.queue,
          request.jobId,
          request.expectedState,
          String(request.availableAtMs),
          String(request.resetDeliveries),
        ].join('\0'),
      )
      .digest('base64url')}`;

    await this.retryUntil(
      () =>
        this.call<{
          mutationSequence: number | null;
          committed: boolean;
          sequence: number | null;
          updated: boolean;
          resumed: boolean;
        }>({
          op: 'requeue',
          queue: request.queue,
          job_id: request.jobId,
          operation_id: operationId,
          expected_status: request.expectedState,
          available_at_ms: Math.max(0, Math.trunc(request.availableAtMs)),
          reset_deliveries: request.resetDeliveries,
          partition: this.partition,
          replication_factor: this.replicationFactor,
        }),
      value => value.committed && value.updated,
      `requeue ${request.queue}/${request.jobId}`,
    );
  }

  async delay(
    queue: string,
    jobId: string,
    availableAtMs: number,
  ): Promise<void> {
    await this.reschedule(queue, jobId, availableAtMs, 'delay');
  }

  async retry(queue: string, jobId: string): Promise<void> {
    await this.reschedule(queue, jobId, Date.now(), 'retry');
  }

  async promote(queue: string, jobId: string): Promise<void> {
    await this.reschedule(queue, jobId, Date.now(), 'promote');
  }

  async reapExpired(queue: string, nowMs: number): Promise<string[]> {
    await this.ensureQueue(queue);
    const expired: string[] = [];
    const deadline = Date.now() + this.operationTimeoutMs;

    for (;;) {
      const result = await this.call<{
        mutationSequence: number | null;
        committed: boolean;
        expiredSequence: number | null;
        transition: null | {
          status: NulangQueueState;
          deliveries: number;
          availableAtMs: number | null;
        };
      }>({
        op: 'reap',
        queue,
        partition: this.partition,
        replication_factor: this.replicationFactor,
        now_ms: nowMs,
      });

      if (result.expiredSequence == null) {
        return expired;
      }
      if (result.transition) {
        expired.push(String(result.expiredSequence));
        continue;
      }
      await this.sleepUntilRetry(deadline, `reap expired ${queue}`);
    }
  }

  async getJob(
    queue: string,
    jobId: string,
  ): Promise<NulangQueueJob | undefined> {
    try {
      const job = await this.call<null | {
        sequence: number;
        jobId: string;
        name: string;
        payloadHex: string;
        priority: number;
        status: NulangQueueState;
        deliveries: number;
        availableAtMs: number;
        leaseUntilMs: number | null;
        lastError: string | null;
        resultHex: string | null;
      }>({
        op: 'job',
        queue,
        job_id: jobId,
      });
      if (!job) {
        return undefined;
      }
      return {
        jobId: job.jobId,
        sequence: job.sequence,
        name: job.name,
        payload: fromHex(job.payloadHex),
        priority: job.priority,
        state: job.status,
        deliveries: job.deliveries,
        availableAtMs: job.availableAtMs,
        failedReason: job.lastError ?? undefined,
        returnValue:
          job.resultHex == null
            ? undefined
            : JSON.parse(new TextDecoder().decode(fromHex(job.resultHex))),
      };
    } catch (error) {
      if (error instanceof NulangQueueApiError && error.status === 404) {
        return undefined;
      }
      throw error;
    }
  }

  async getState(
    queue: string,
    jobId: string,
  ): Promise<NulangQueueState | undefined> {
    return (await this.getJob(queue, jobId))?.state;
  }

  async getCounts(queue: string): Promise<NulangQueueCounts> {
    const info = await this.call<{
      waiting: number;
      active: number;
      completed: number;
      failed: number;
      deadLettered: number;
    }>({
      op: 'info',
      queue,
    });
    return {
      waiting: info.waiting,
      active: info.active,
      completed: info.completed,
      failed: info.failed,
      delayed: 0,
      prioritized: 0,
      deadLettered: info.deadLettered,
    };
  }

  async setQueueMeta(
    queue: string,
    values: Record<string, string | number>,
  ): Promise<number> {
    const current = this.metadata.get(queue) ?? {};
    for (const [key, value] of Object.entries(values)) {
      current[key] = String(value);
    }
    this.metadata.set(queue, current);
    return Object.keys(values).length;
  }

  async getQueueMeta(queue: string): Promise<Record<string, string>> {
    return { ...(this.metadata.get(queue) ?? {}) };
  }

  async removeQueueMetaFields(
    queue: string,
    fields: string[],
  ): Promise<number> {
    const current = this.metadata.get(queue) ?? {};
    let removed = 0;
    for (const field of fields) {
      if (Object.hasOwn(current, field)) {
        delete current[field];
        removed += 1;
      }
    }
    this.metadata.set(queue, current);
    return removed;
  }

  async waitForJob(
    queue: string,
    blockTimeoutMs: number,
  ): Promise<NulangQueueWaitSignal | null> {
    const generation = this.blockingGeneration;
    const deadline = Date.now() + Math.max(0, blockTimeoutMs);

    while (!this.closed && generation === this.blockingGeneration) {
      const now = Date.now();
      const signal = await this.call<{
        ready: boolean;
        nextAvailableAtMs: number | null;
      }>({
        op: 'ready',
        queue,
        now_ms: now,
      });

      if (signal.ready) {
        return { member: queue, score: 0 };
      }
      if (signal.nextAvailableAtMs != null) {
        return { member: queue, score: signal.nextAvailableAtMs };
      }
      if (now >= deadline) {
        return null;
      }
      await sleep(Math.min(this.pollIntervalMs, Math.max(1, deadline - now)));
    }
    return null;
  }

  async disconnectBlocking(): Promise<void> {
    this.blockingGeneration += 1;
  }

  async reconnectBlocking(): Promise<void> {
    // A subsequent wait captures the new generation.
  }

  private async reschedule(
    queue: string,
    jobId: string,
    availableAtMs: number,
    kind: 'delay' | 'retry' | 'promote',
  ): Promise<void> {
    await this.ensureQueue(queue);
    const operationId = `bull:reschedule:${createHash('sha256')
      .update([queue, jobId, kind, String(availableAtMs)].join('\0'))
      .digest('base64url')}`;

    await this.retryUntil(
      () =>
        this.call<{
          mutationSequence: number | null;
          committed: boolean;
          sequence: number | null;
          availableAtMs: number;
          updated: boolean;
          resumed: boolean;
        }>({
          op: 'reschedule',
          queue,
          job_id: jobId,
          operation_id: operationId,
          available_at_ms: Math.max(0, Math.trunc(availableAtMs)),
          partition: this.partition,
          replication_factor: this.replicationFactor,
        }),
      value => value.committed && value.updated,
      `${kind} ${queue}/${jobId}`,
    );
  }

  private async ensureQueue(queue: string): Promise<void> {
    if (this.ensuredQueues.has(queue)) {
      return;
    }
    await this.retryUntil(
      () =>
        this.call<{
          created: boolean;
          policyReady: boolean;
          committed: boolean;
        }>({
          op: 'ensure',
          queue,
          partition: this.partition,
          replication_factor: this.replicationFactor,
          visibility_timeout_ms: this.visibilityTimeoutMs,
          max_attempts: this.queueMaxAttempts,
        }),
      value => value.created && value.policyReady,
      `ensure queue ${queue}`,
    );
    this.ensuredQueues.add(queue);
  }

  private async retryUntil<T>(
    operation: () => Promise<T>,
    done: (value: T) => boolean,
    label: string,
  ): Promise<T> {
    const deadline = Date.now() + this.operationTimeoutMs;
    for (;;) {
      this.assertOpen();
      const value = await operation();
      if (done(value)) {
        return value;
      }
      await this.sleepUntilRetry(deadline, label);
    }
  }

  private async sleepUntilRetry(deadline: number, label: string): Promise<void> {
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      throw new Error(`Timed out waiting for Nulang queue operation: ${label}`);
    }
    await sleep(Math.min(this.pollIntervalMs, remaining));
  }

  private async call<T>(payload: Record<string, unknown>): Promise<T> {
    this.assertOpen();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.operationTimeoutMs);
    try {
      const response = await fetch(`${this.endpoint}/v1/queue`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          ...(this.connectionName
            ? { 'x-nulang-client-name': this.connectionName }
            : {}),
        },
        body: JSON.stringify(payload),
        signal: controller.signal,
      });
      const envelope = (await response.json()) as ApiEnvelope<T>;
      if (!response.ok || !envelope.ok || envelope.result === undefined) {
        throw new NulangQueueApiError(
          response.status,
          envelope.error?.kind ?? 'http_error',
          envelope.error?.message ??
            `Nulang queue API returned HTTP ${response.status}`,
        );
      }
      return envelope.result;
    } finally {
      clearTimeout(timer);
    }
  }

  private assertOpen(): void {
    if (this.closed) {
      throw new Error('Nulang HTTP queue client is closed');
    }
  }
}

function toHex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString('hex');
}

function fromHex(value: string): Uint8Array {
  return new Uint8Array(Buffer.from(value, 'hex'));
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}
