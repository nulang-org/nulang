export class NulangBullMQUnsupportedError extends Error {
  readonly code = 'ERR_NULANG_BULLMQ_UNSUPPORTED';

  constructor(public readonly operation: string) {
    super(
      `BullMQ operation "${operation}" is not implemented by @nulang/bullmq-backend B1`,
    );
    this.name = 'NulangBullMQUnsupportedError';
  }
}

export function unsupported(operation: string): never {
  throw new NulangBullMQUnsupportedError(operation);
}
