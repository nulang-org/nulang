import type { JobJson, JobState, JobType } from 'bullmq';

export type NulangQueueState =
  | 'waiting'
  | 'active'
  | 'completed'
  | 'failed'
  | 'dead-lettered';

export interface NulangQueueAddRequest {
  queue: string;
  jobId: string;
  name: string;
  payload: Uint8Array;
  priority: number;
  delayMs: number;
  timestampMs: number;
  maxAttempts: number;
}

export interface NulangQueueAddResult {
  jobId: string;
  sequence: number;
  deduplicated: boolean;
  enqueued: boolean;
}

export interface NulangQueueAcquireRequest {
  queue: string;
  consumer: string;
  operationId: string;
  nowMs: number;
}

export interface NulangQueueDelivery {
  queue: string;
  sequence: number;
  jobId: string;
  name: string;
  payload: Uint8Array;
  priority: number;
  deliveries: number;
  queueEpoch: number;
  leaseToken: number;
  leaseUntilMs: number;
}

export interface NulangQueueLeaseRef {
  queue: string;
  sequence: number;
  jobId: string;
  consumer: string;
  queueEpoch: number;
  leaseToken: number;
  operationId: string;
}

export interface NulangQueueJob {
  jobId: string;
  sequence: number;
  name: string;
  payload: Uint8Array;
  priority: number;
  state: NulangQueueState;
  deliveries: number;
  availableAtMs?: number;
  processedOn?: number;
  finishedOn?: number;
  returnValue?: unknown;
  failedReason?: string;
}

export interface NulangQueueCounts {
  waiting: number;
  active: number;
  completed: number;
  failed: number;
  delayed: number;
  prioritized: number;
  deadLettered: number;
}

export interface NulangQueueFailRequest extends NulangQueueLeaseRef {
  failedReason: string;
  delayMs: number;
  terminal: boolean;
  nowMs: number;
}

export interface NulangQueueCompleteRequest extends NulangQueueLeaseRef {
  returnValue: unknown;
  nowMs: number;
}

export interface NulangQueueRenewRequest extends NulangQueueLeaseRef {
  extensionMs: number;
  nowMs: number;
}

export interface NulangQueueWaitSignal {
  member: string;
  score: number;
}

export interface NulangQueueClient {
  waitUntilReady(): Promise<void>;
  close(force?: boolean): Promise<void>;
  disconnect(): Promise<void>;
  setName?(name: string): Promise<void>;

  add(request: NulangQueueAddRequest): Promise<NulangQueueAddResult>;
  addMany?(requests: NulangQueueAddRequest[]): Promise<NulangQueueAddResult[]>;

  acquire(request: NulangQueueAcquireRequest): Promise<NulangQueueDelivery | null>;
  complete(request: NulangQueueCompleteRequest): Promise<void>;
  fail(request: NulangQueueFailRequest): Promise<void>;
  renew(request: NulangQueueRenewRequest): Promise<number>;

  delay(queue: string, jobId: string, availableAtMs: number): Promise<void>;
  retry(queue: string, jobId: string): Promise<void>;
  promote(queue: string, jobId: string): Promise<void>;
  reapExpired(queue: string, nowMs: number): Promise<string[]>;

  getJob(queue: string, jobId: string): Promise<NulangQueueJob | undefined>;
  getState(queue: string, jobId: string): Promise<NulangQueueState | undefined>;
  getCounts(queue: string): Promise<NulangQueueCounts>;
  getCountsPerPriority?(queue: string, priorities: number[]): Promise<number[]>;

  setQueueMeta(
    queue: string,
    values: Record<string, string | number>,
  ): Promise<number>;
  getQueueMeta(queue: string): Promise<Record<string, string>>;
  removeQueueMetaFields(queue: string, fields: string[]): Promise<number>;

  waitForJob(
    queue: string,
    blockTimeoutMs: number,
  ): Promise<NulangQueueWaitSignal | null>;
  disconnectBlocking?(): Promise<void>;
  reconnectBlocking?(): Promise<void>;
}

export interface EncodedBullMQJob {
  version: 1;
  job: JobJson;
}

export function encodeBullMQJob(job: JobJson): Uint8Array {
  return new TextEncoder().encode(
    JSON.stringify({
      version: 1,
      job,
    } satisfies EncodedBullMQJob),
  );
}

export function decodeBullMQJob(payload: Uint8Array): JobJson {
  const parsed = JSON.parse(new TextDecoder().decode(payload)) as EncodedBullMQJob;
  if (parsed?.version !== 1 || !parsed.job || typeof parsed.job !== 'object') {
    throw new Error('invalid @nulang/bullmq-backend job envelope');
  }
  return parsed.job;
}

export function toBullMQState(
  state: NulangQueueState | undefined,
  priority = 0,
): JobState | 'unknown' {
  switch (state) {
    case 'waiting':
      return priority > 0 ? 'prioritized' : 'waiting';
    case 'active':
      return 'active';
    case 'completed':
      return 'completed';
    case 'failed':
    case 'dead-lettered':
      return 'failed';
    default:
      return 'unknown';
  }
}

export function countForBullMQType(
  counts: NulangQueueCounts,
  type: JobType,
): number {
  switch (type) {
    case 'wait':
    case 'waiting':
      return counts.waiting;
    case 'active':
      return counts.active;
    case 'completed':
      return counts.completed;
    case 'failed':
      return counts.failed + counts.deadLettered;
    case 'delayed':
      return counts.delayed;
    case 'prioritized':
      return counts.prioritized;
    default:
      return 0;
  }
}
