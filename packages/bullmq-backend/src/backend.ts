import { createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';

import type {
  BackendFactory,
  IQueueBackend,
  JobJson,
  JobState,
  JobType,
  KeepJobs,
  KeysMap,
  MinimalJob,
  ParentKeyOpts,
  QueueBaseOptions,
} from 'bullmq';

import {
  countForBullMQType,
  decodeBullMQJob,
  encodeBullMQJob,
  toBullMQState,
  type NulangQueueClient,
  type NulangQueueDelivery,
  type NulangQueueJob,
  type NulangQueueLeaseRef,
} from './client.js';
import { unsupported } from './errors.js';

const BULLMQ_MAX_PRIORITY = 2_097_151;
const NATIVE_PRIORITY_ZERO = BULLMQ_MAX_PRIORITY + 1;
const SAFE_META_FIELDS = new Set(['version', 'opts.maxLenEvents']);

const KEY_TYPES = [
  '',
  'active',
  'wait',
  'waiting-children',
  'paused',
  'id',
  'delayed',
  'prioritized',
  'stalled-check',
  'completed',
  'failed',
  'stalled',
  'repeat',
  'limiter',
  'meta',
  'events',
  'pc',
  'marker',
  'de',
] as const;

export interface NulangQueueBackendOptions {
  minimumBlockTimeout?: number;
  maximumBlockTimeout?: number;
  now?: () => number;
}

interface ActiveLease {
  bullToken: string;
  lease: NulangQueueLeaseRef;
  delivery: NulangQueueDelivery;
  renewCounter: number;
}

export class NulangQueueSession {
  private references = 0;
  private closed = false;

  constructor(readonly client: NulangQueueClient) {}

  retain(): void {
    if (this.closed) {
      throw new Error('Nulang queue session is already closed');
    }
    this.references += 1;
  }

  async release(force?: boolean): Promise<void> {
    if (this.references > 0) {
      this.references -= 1;
    }
    if (this.references === 0 && !this.closed) {
      this.closed = true;
      await this.client.close(force);
    }
  }

  async disconnect(): Promise<void> {
    if (!this.closed) {
      this.closed = true;
      await this.client.disconnect();
    }
  }
}

export class NulangQueueBackend
  extends EventEmitter
  implements IQueueBackend
{
  readonly qualifiedName: string;
  readonly keys: KeysMap;
  readonly minimumBlockTimeout: number;
  readonly maximumBlockTimeout: number;

  closing: Promise<void> | undefined;

  private readonly now: () => number;
  private ready = false;
  private released = false;
  private connectionName?: string;
  private acquireCounter = 0;
  private readonly pendingAcquireIds = new Map<string, string>();
  private readonly activeLeases = new Map<string, ActiveLease>();

  constructor(
    private readonly session: NulangQueueSession,
    readonly queueName: string,
    readonly opts: QueueBaseOptions,
    options: NulangQueueBackendOptions = {},
  ) {
    super();
    this.session.retain();
    this.qualifiedName = queueName;
    this.minimumBlockTimeout = options.minimumBlockTimeout ?? 0.001;
    this.maximumBlockTimeout = options.maximumBlockTimeout ?? 300;
    this.now = options.now ?? Date.now;
    this.keys = Object.fromEntries(
      KEY_TYPES.map(type => [type, this.toKey(type)]),
    );
  }

  async waitUntilReady(): Promise<void> {
    await this.session.client.waitUntilReady();
    if (!this.ready) {
      this.ready = true;
      this.emit('ready');
    }
  }

  async close(force?: boolean): Promise<void> {
    if (!this.closing) {
      this.closing = (async () => {
        if (!this.released) {
          this.released = true;
          await this.session.release(force);
        }
        this.emit('close');
      })();
    }
    await this.closing;
  }

  async disconnect(): Promise<void> {
    await this.session.disconnect();
    this.emit('close');
  }

  async setName(name: string): Promise<void> {
    this.connectionName = name;
    await this.session.client.setName?.(name);
  }

  forQueue(queueName: string, _prefix?: string): IQueueBackend {
    return new NulangQueueBackend(this.session, queueName, this.opts, {
      minimumBlockTimeout: this.minimumBlockTimeout,
      maximumBlockTimeout: this.maximumBlockTimeout,
      now: this.now,
    });
  }

  toKey(type: string): string {
    return `${this.qualifiedName}:${type}`;
  }

  parseNodeKey(key: string): { prefix: string; queueName: string; id: string } {
    const separator = key.indexOf(':');
    if (separator <= 0 || separator === key.length - 1) {
      throw new Error(`Invalid Nulang BullMQ node key: ${key}`);
    }
    return {
      prefix: '',
      queueName: key.slice(0, separator),
      id: key.slice(separator + 1),
    };
  }

  clientName(suffix = ''): string {
    if (this.connectionName) {
      return `${this.connectionName}${suffix}`;
    }
    return `nulang:${Buffer.from(this.queueName).toString('base64url')}${suffix}`;
  }

  async addJob(
    job: JobJson,
    jobId: string,
    parentKeyOpts: ParentKeyOpts = {},
  ): Promise<string> {
    this.assertIndependentJob(job, parentKeyOpts);
    const request = this.toAddRequest(job, jobId);
    const result = await this.session.client.add(request);
    if (!result.enqueued) {
      throw new Error(
        `Nulang Fabric Queue did not quorum-commit BullMQ job ${jobId}`,
      );
    }
    return result.jobId;
  }

  async addJobs(
    entries: {
      job: JobJson;
      jobId: string;
      parentKeyOpts?: ParentKeyOpts;
    }[],
  ): Promise<string[]> {
    const requests = entries.map(({ job, jobId, parentKeyOpts }) => {
      this.assertIndependentJob(job, parentKeyOpts ?? {});
      return this.toAddRequest(job, jobId);
    });

    if (this.session.client.addMany) {
      const results = await this.session.client.addMany(requests);
      if (results.length !== entries.length || results.some(r => !r.enqueued)) {
        throw new Error('Nulang Fabric Queue did not commit the complete BullMQ batch');
      }
      return results.map(r => r.jobId);
    }

    const ids: string[] = [];
    for (const request of requests) {
      const result = await this.session.client.add(request);
      if (!result.enqueued) {
        throw new Error(
          `Nulang Fabric Queue did not quorum-commit BullMQ job ${request.jobId}`,
        );
      }
      ids.push(result.jobId);
    }
    return ids;
  }

  async moveToActive(token: string, name?: string): Promise<any[]> {
    const operationKey = `${this.queueName}\0${token}`;
    let operationId = this.pendingAcquireIds.get(operationKey);
    if (!operationId) {
      operationId = this.operationId(
        'acquire',
        token,
        String(this.lockDuration),
        String(this.acquireCounter++),
      );
      this.pendingAcquireIds.set(operationKey, operationId);
    }

    try {
      const consumer = this.consumerId(token);
      const delivery = await this.session.client.acquire({
        queue: this.queueName,
        consumer,
        operationId,
        leaseDurationMs: this.lockDuration,
        nowMs: this.now(),
      });
      this.pendingAcquireIds.delete(operationKey);

      if (!delivery) {
        return [null, '', 0, 0];
      }

      const job = this.jobJsonForDelivery(delivery, name);
      this.activeLeases.set(delivery.jobId, {
        bullToken: token,
        lease: {
          queue: this.queueName,
          sequence: delivery.sequence,
          jobId: delivery.jobId,
          consumer,
          queueEpoch: delivery.queueEpoch,
          leaseToken: delivery.leaseToken,
          operationId,
        },
        delivery,
        renewCounter: 0,
      });
      return [job, delivery.jobId, 0, 0];
    } catch (error) {
      // Preserve the operation id so a transport retry resumes the same
      // Fabric lease mutation rather than appending another one.
      throw error;
    }
  }

  async moveToCompleted<T = any, R = any, N extends string = string>(
    job: MinimalJob<T, R, N>,
    returnValue: R,
    removeOnComplete: boolean | number | KeepJobs,
    token: string,
    fetchNext: boolean,
  ): Promise<{ result: void | any[]; finishedOn: number }> {
    this.assertRetentionUnsupported(removeOnComplete, 'moveToCompleted');
    const id = this.requireJobId(job.id);
    const active = this.requireActiveLease(id, token);
    const finishedOn = this.now();

    await this.session.client.complete({
      ...active.lease,
      operationId: this.operationId(
        'complete',
        id,
        token,
        String(active.lease.leaseToken),
      ),
      returnValue,
      nowMs: finishedOn,
    });
    this.activeLeases.delete(id);

    return {
      result: fetchNext ? await this.moveToActive(token) : undefined,
      finishedOn,
    };
  }

  async moveToFailed<T = any, R = any, N extends string = string>(
    job: MinimalJob<T, R, N>,
    failedReason: string,
    removeOnFail: boolean | number | KeepJobs,
    token: string,
    fetchNext: boolean,
    fieldsToUpdate?: Record<string, any>,
  ): Promise<{ result: void | any[]; finishedOn: number }> {
    this.assertRetentionUnsupported(removeOnFail, 'moveToFailed');
    const id = this.requireJobId(job.id);
    const active = this.requireActiveLease(id, token);
    const finishedOn = this.now();

    await this.session.client.fail({
      ...active.lease,
      operationId: this.operationId(
        'fail',
        id,
        token,
        String(active.lease.leaseToken),
      ),
      failedReason,
      fieldsToUpdate,
      delayMs: 0,
      terminal: true,
      nowMs: finishedOn,
    });
    this.activeLeases.delete(id);

    return {
      result: fetchNext ? await this.moveToActive(token) : undefined,
      finishedOn,
    };
  }

  async moveToDelayed(
    jobId: string,
    timestamp: number,
    delay: number,
    token?: string,
    opts?: { fieldsToUpdate?: Record<string, any>; fetchNext?: boolean },
  ): Promise<void | any[]> {
    const active = token ? this.matchActiveLease(jobId, token) : undefined;
    if (active && token) {
      await this.session.client.fail({
        ...active.lease,
        operationId: this.operationId(
          'delay',
          jobId,
          token,
          String(active.lease.leaseToken),
        ),
        failedReason:
          String(opts?.fieldsToUpdate?.failedReason ?? 'BullMQ delayed retry'),
        fieldsToUpdate: opts?.fieldsToUpdate,
        delayMs: Math.max(0, delay),
        terminal: false,
        nowMs: timestamp,
      });
      this.activeLeases.delete(jobId);
      return opts?.fetchNext ? this.moveToActive(token) : undefined;
    }

    await this.session.client.delay(
      this.queueName,
      jobId,
      timestamp + Math.max(0, delay),
    );
    return undefined;
  }

  async moveJobFromActiveToWait(
    jobId: string,
    token?: string,
  ): Promise<number> {
    if (!token) {
      return 0;
    }
    const active = this.matchActiveLease(jobId, token);
    if (!active) {
      return 0;
    }
    await this.session.client.fail({
      ...active.lease,
      operationId: this.operationId(
        'wait',
        jobId,
        token,
        String(active.lease.leaseToken),
      ),
      failedReason: 'BullMQ moved active job to wait',
      delayMs: 0,
      terminal: false,
      nowMs: this.now(),
    });
    this.activeLeases.delete(jobId);
    return 1;
  }

  async retryJob(
    jobId: string,
    lifo: boolean,
    token?: string,
    opts?: { fieldsToUpdate?: Record<string, any> },
  ): Promise<void> {
    if (lifo) {
      unsupported('retryJob(lifo=true)');
    }
    const active = token ? this.matchActiveLease(jobId, token) : undefined;
    if (active && token) {
      await this.session.client.fail({
        ...active.lease,
        operationId: this.operationId(
          'retry',
          jobId,
          token,
          String(active.lease.leaseToken),
        ),
        failedReason:
          String(opts?.fieldsToUpdate?.failedReason ?? 'BullMQ immediate retry'),
        fieldsToUpdate: opts?.fieldsToUpdate,
        delayMs: 0,
        terminal: false,
        nowMs: this.now(),
      });
      this.activeLeases.delete(jobId);
      return;
    }
    await this.session.client.retry(this.queueName, jobId);
  }

  async promote(jobId: string): Promise<void> {
    await this.session.client.promote(this.queueName, jobId);
  }

  async moveStalledJobsToWait(): Promise<string[]> {
    return this.session.client.reapExpired(this.queueName, this.now());
  }

  async extendLock(
    jobId: string,
    token: string,
    duration: number,
  ): Promise<number> {
    const active = this.matchActiveLease(jobId, token);
    if (!active) {
      return 0;
    }
    const counter = active.renewCounter;
    const deadline = await this.session.client.renew({
      ...active.lease,
      operationId: this.operationId(
        'renew',
        jobId,
        token,
        String(active.lease.leaseToken),
        String(counter),
      ),
      extensionMs: duration,
      nowMs: this.now(),
    });
    active.renewCounter += 1;
    active.delivery.leaseUntilMs = deadline;
    return 1;
  }

  async extendLocks(
    jobIds: string[],
    tokens: string[],
    duration: number,
  ): Promise<string[]> {
    const failed: string[] = [];
    for (let index = 0; index < jobIds.length; index += 1) {
      const jobId = jobIds[index];
      const token = tokens[index];
      if (!jobId || !token || (await this.extendLock(jobId, token, duration)) !== 1) {
        if (jobId) {
          failed.push(jobId);
        }
      }
    }
    return failed;
  }

  async waitForJob(
    blockTimeout: number,
  ): Promise<{ member: string; score: number } | null> {
    return this.session.client.waitForJob(
      this.queueName,
      Math.max(0, blockTimeout) * 1000,
    );
  }

  async disconnectBlocking(_wait = true): Promise<void> {
    await this.session.client.disconnectBlocking?.();
  }

  async reconnectBlocking(): Promise<void> {
    await this.session.client.reconnectBlocking?.();
  }

  async getState(jobId: string): Promise<JobState | 'unknown'> {
    const job = await this.session.client.getJob(this.queueName, jobId);
    if (!job) {
      return 'unknown';
    }
    if (job.state === 'waiting' && (job.availableAtMs ?? 0) > this.now()) {
      return 'delayed';
    }
    const decoded = decodeBullMQJob(job.payload);
    const priority = Number(decoded.priority ?? decoded.opts?.priority ?? 0);
    return toBullMQState(job.state, priority);
  }

  async isFinished(
    jobId: string,
    returnValue = false,
  ): Promise<number | [number, string]> {
    const job = await this.session.client.getJob(this.queueName, jobId);
    let status = 0;
    let value = '';

    if (!job) {
      status = -1;
      value = `Missing job ${this.toKey(jobId)}`;
    } else if (job.state === 'completed') {
      status = 1;
      value = JSON.stringify(job.returnValue ?? null);
    } else if (job.state === 'failed' || job.state === 'dead-lettered') {
      status = 2;
      value = job.failedReason ?? '';
    }

    return returnValue ? [status, value] : status;
  }

  async isMaxed(): Promise<boolean> {
    const meta = await this.session.client.getQueueMeta(this.queueName);
    if (meta.concurrency == null) {
      return false;
    }
    unsupported('global concurrency');
  }

  async isJobInState(state: string, jobId: string): Promise<boolean> {
    const current = await this.getState(jobId);
    if (state === 'wait') {
      return current === 'waiting';
    }
    return current === state;
  }

  async getJobData(jobId: string): Promise<JobJson | undefined> {
    const job = await this.session.client.getJob(this.queueName, jobId);
    return job ? this.jobJsonForStoredJob(job) : undefined;
  }

  async getCounts(types: JobType[]): Promise<number[]> {
    const counts = await this.session.client.getCounts(this.queueName);
    return types.map(type => countForBullMQType(counts, type));
  }

  async getCountsPerPriority(priorities: number[]): Promise<number[]> {
    if (!this.session.client.getCountsPerPriority) {
      return unsupported('getCountsPerPriority');
    }
    return this.session.client.getCountsPerPriority(this.queueName, priorities);
  }

  async getClientList(): Promise<string[]> {
    return [];
  }

  async setQueueMeta(values: Record<string, string | number>): Promise<number> {
    this.assertSafeMeta(values);
    return this.session.client.setQueueMeta(this.queueName, values);
  }

  async getQueueMetaField(field: string): Promise<string | null> {
    const meta = await this.session.client.getQueueMeta(this.queueName);
    return meta[field] ?? null;
  }

  async getQueueMetaFields(fields: string[]): Promise<(string | null)[]> {
    const meta = await this.session.client.getQueueMeta(this.queueName);
    return fields.map(field => meta[field] ?? null);
  }

  async getQueueMeta(): Promise<Record<string, string>> {
    return this.session.client.getQueueMeta(this.queueName);
  }

  async removeQueueMetaFields(fields: string[]): Promise<number> {
    for (const field of fields) {
      if (!SAFE_META_FIELDS.has(field)) {
        unsupported(`removeQueueMetaFields(${field})`);
      }
    }
    return this.session.client.removeQueueMetaFields(this.queueName, fields);
  }

  async hasQueueMetaField(field: string): Promise<boolean> {
    const meta = await this.session.client.getQueueMeta(this.queueName);
    return Object.hasOwn(meta, field);
  }

  async addFlow(..._args: any[]): Promise<any> {
    return unsupported('addFlow');
  }
  async addJobScheduler(..._args: any[]): Promise<any> {
    return unsupported('addJobScheduler');
  }
  async moveToWaitingChildren(..._args: any[]): Promise<any> {
    return unsupported('moveToWaitingChildren');
  }
  async retryFinishedJob<T = any, R = any, N extends string = string>(
    job: MinimalJob<T, R, N>,
    state: 'failed' | 'completed',
    opts: {
      resetAttemptsMade?: boolean;
      resetAttemptsStarted?: boolean;
    } = {},
  ): Promise<void> {
    const resetMade = opts.resetAttemptsMade ?? false;
    const resetStarted = opts.resetAttemptsStarted ?? false;
    if (resetMade !== resetStarted) {
      unsupported('retryFinishedJob(asymmetric attempt reset)');
    }
    await this.session.client.requeue({
      queue: this.queueName,
      jobId: this.requireJobId(job.id),
      expectedState: state,
      availableAtMs: this.now(),
      resetDeliveries: resetMade && resetStarted,
    });
  }
  async retryFinishedJobs(..._args: any[]): Promise<any> {
    return unsupported('retryFinishedJobs');
  }
  async promoteJobs(..._args: any[]): Promise<any> {
    return unsupported('promoteJobs');
  }
  async pause(..._args: any[]): Promise<any> {
    return unsupported('pause');
  }
  async drain(..._args: any[]): Promise<any> {
    return unsupported('drain');
  }
  async cleanJobsByState(..._args: any[]): Promise<any> {
    return unsupported('cleanJobsByState');
  }
  async obliterate(..._args: any[]): Promise<any> {
    return unsupported('obliterate');
  }
  async removeOrphanedJobs(..._args: any[]): Promise<any> {
    return unsupported('removeOrphanedJobs');
  }
  async updateData(..._args: any[]): Promise<any> {
    return unsupported('updateData');
  }
  async updateProgress(..._args: any[]): Promise<any> {
    return unsupported('updateProgress');
  }
  async addLog(..._args: any[]): Promise<any> {
    return unsupported('addLog');
  }
  async clearLogs(..._args: any[]): Promise<any> {
    return unsupported('clearLogs');
  }
  async changeDelay(jobId: string, delay: number): Promise<void> {
    await this.session.client.delay(
      this.queueName,
      jobId,
      this.now() + Math.max(0, delay),
    );
  }
  async changePriority(..._args: any[]): Promise<any> {
    return unsupported('changePriority');
  }
  async remove(..._args: any[]): Promise<any> {
    return unsupported('remove');
  }
  async removeUnprocessedChildren(..._args: any[]): Promise<any> {
    return unsupported('removeUnprocessedChildren');
  }
  async removeChildDependency(..._args: any[]): Promise<any> {
    return unsupported('removeChildDependency');
  }
  async removeDeduplicationKey(..._args: any[]): Promise<any> {
    return unsupported('removeDeduplicationKey');
  }
  async deleteDeduplicationKey(..._args: any[]): Promise<any> {
    return unsupported('deleteDeduplicationKey');
  }
  async updateJobSchedulerNextMillis(..._args: any[]): Promise<any> {
    return unsupported('updateJobSchedulerNextMillis');
  }
  async removeJobScheduler(..._args: any[]): Promise<any> {
    return unsupported('removeJobScheduler');
  }
  async getJobScheduler(..._args: any[]): Promise<any> {
    return unsupported('getJobScheduler');
  }
  async isJobScheduler(..._args: any[]): Promise<any> {
    return unsupported('isJobScheduler');
  }
  async getJobSchedulerData(..._args: any[]): Promise<any> {
    return unsupported('getJobSchedulerData');
  }
  async getJobSchedulersRange(..._args: any[]): Promise<any> {
    return unsupported('getJobSchedulersRange');
  }
  async getJobSchedulersCount(..._args: any[]): Promise<any> {
    return unsupported('getJobSchedulersCount');
  }
  async getDeduplicationJobId(..._args: any[]): Promise<any> {
    return unsupported('getDeduplicationJobId');
  }
  async getJobLogs(..._args: any[]): Promise<any> {
    return unsupported('getJobLogs');
  }
  async getRateLimitTtl(..._args: any[]): Promise<any> {
    return unsupported('getRateLimitTtl');
  }
  async getRanges(..._args: any[]): Promise<any> {
    return unsupported('getRanges');
  }
  async getDependencyCounts(..._args: any[]): Promise<any> {
    return unsupported('getDependencyCounts');
  }
  async getDependencies(..._args: any[]): Promise<any> {
    return unsupported('getDependencies');
  }
  async getProcessedChildrenValues(..._args: any[]): Promise<any> {
    return unsupported('getProcessedChildrenValues');
  }
  async getIgnoredChildrenFailures(..._args: any[]): Promise<any> {
    return unsupported('getIgnoredChildrenFailures');
  }
  async getMetrics(..._args: any[]): Promise<any> {
    return unsupported('getMetrics');
  }
  async paginate(..._args: any[]): Promise<any> {
    return unsupported('paginate');
  }
  async setRateLimit(..._args: any[]): Promise<any> {
    return unsupported('setRateLimit');
  }
  async removeRateLimitKey(..._args: any[]): Promise<any> {
    return unsupported('removeRateLimitKey');
  }
  async removeDeprecatedPriorityKey(..._args: any[]): Promise<any> {
    return unsupported('removeDeprecatedPriorityKey');
  }
  async trimEvents(..._args: any[]): Promise<any> {
    return unsupported('trimEvents');
  }
  async publishEvent(..._args: any[]): Promise<any> {
    return unsupported('publishEvent');
  }
  async readEvents(..._args: any[]): Promise<any> {
    return unsupported('readEvents');
  }

  private get lockDuration(): number {
    const duration = Number((this.opts as any).lockDuration ?? 30_000);
    if (!Number.isFinite(duration) || duration <= 0) {
      throw new Error('BullMQ lockDuration must be a positive number');
    }
    return duration;
  }

  private assertIndependentJob(
    job: JobJson,
    parentKeyOpts: ParentKeyOpts,
  ): void {
    const opts = (job.opts ?? {}) as Record<string, any>;
    if (
      parentKeyOpts.parentKey ||
      parentKeyOpts.addToWaitingChildren ||
      job.parent ||
      job.parentKey
    ) {
      unsupported('job dependencies/flows');
    }
    if (job.repeatJobKey || opts.repeat) {
      unsupported('repeat/scheduler jobs');
    }
    if (job.deduplicationId || opts.deduplication || job.debounceId) {
      unsupported('BullMQ deduplication keys');
    }
    if (opts.lifo) {
      unsupported('lifo');
    }
  }

  private toAddRequest(job: JobJson, jobId: string) {
    const opts = (job.opts ?? {}) as Record<string, any>;
    const priority = Number(job.priority ?? opts.priority ?? 0);
    return {
      queue: this.queueName,
      jobId: jobId || job.id,
      name: job.name,
      payload: encodeBullMQJob(job),
      priority: this.priorityToNative(priority),
      delayMs: Math.max(0, Number(job.delay ?? opts.delay ?? 0)),
      timestampMs: Number(job.timestamp ?? this.now()),
      maxAttempts: Math.max(1, Number(opts.attempts ?? 1)),
    };
  }

  private priorityToNative(priority: number): number {
    if (
      !Number.isInteger(priority) ||
      priority < 0 ||
      priority > BULLMQ_MAX_PRIORITY
    ) {
      throw new RangeError(
        `BullMQ priority must be an integer between 0 and ${BULLMQ_MAX_PRIORITY}`,
      );
    }
    return NATIVE_PRIORITY_ZERO - priority;
  }

  private consumerId(token: string): string {
    if (token.length <= 128) {
      return token;
    }
    return `bull:${this.digest(token).slice(0, 96)}`;
  }

  private operationId(kind: string, ...parts: string[]): string {
    return `bull:${kind}:${this.digest(
      [this.queueName, kind, ...parts].join('\0'),
    )}`;
  }

  private digest(value: string): string {
    return createHash('sha256').update(value).digest('base64url');
  }

  private requireJobId(id: string | undefined): string {
    if (!id) {
      throw new Error('BullMQ job id is required for this Nulang transition');
    }
    return id;
  }

  private requireActiveLease(jobId: string, token: string): ActiveLease {
    const active = this.matchActiveLease(jobId, token);
    if (!active) {
      throw new Error(
        `BullMQ lock token does not own Nulang Fabric Queue job ${jobId}`,
      );
    }
    return active;
  }

  private matchActiveLease(
    jobId: string,
    token: string,
  ): ActiveLease | undefined {
    const active = this.activeLeases.get(jobId);
    return active?.bullToken === token ? active : undefined;
  }

  private jobJsonForDelivery(
    delivery: NulangQueueDelivery,
    workerName?: string,
  ): JobJson {
    const job = decodeBullMQJob(delivery.payload);
    return {
      ...job,
      id: delivery.jobId,
      name: delivery.name,
      processedOn: this.now(),
      delay: 0,
      attemptsMade: Math.max(0, delivery.deliveries - 1),
      attemptsStarted: delivery.deliveries,
      processedBy: workerName ?? job.processedBy,
    };
  }

  private jobJsonForStoredJob(job: NulangQueueJob): JobJson {
    const decoded = decodeBullMQJob(job.payload);
    const active = job.state === 'active';
    return {
      ...decoded,
      id: job.jobId,
      name: job.name,
      attemptsStarted: job.deliveries,
      attemptsMade: active
        ? Math.max(0, job.deliveries - 1)
        : job.deliveries,
      processedOn: job.processedOn ?? decoded.processedOn,
      finishedOn: job.finishedOn ?? decoded.finishedOn,
      failedReason: job.failedReason ?? decoded.failedReason,
      returnvalue:
        job.returnValue === undefined
          ? decoded.returnvalue
          : JSON.stringify(job.returnValue),
    };
  }

  private assertRetentionUnsupported(
    value: boolean | number | KeepJobs | undefined,
    operation: string,
  ): void {
    if (value === undefined || value === false) {
      return;
    }
    unsupported(`${operation} retention/removal policy`);
  }

  private assertSafeMeta(values: Record<string, string | number>): void {
    for (const field of Object.keys(values)) {
      if (!SAFE_META_FIELDS.has(field)) {
        unsupported(`setQueueMeta(${field})`);
      }
    }
  }
}

export function createNulangBackendFactory(
  client: NulangQueueClient,
  options: NulangQueueBackendOptions = {},
): BackendFactory<NulangQueueBackend> {
  const session = new NulangQueueSession(client);
  return (name, opts) =>
    new NulangQueueBackend(session, name, opts, options);
}
