/** Types for the native Fetch implementation, compatible with TypeScript NodeNext. */
export interface OpenApiContract {
  openapi: string;
  paths: Record<string, Record<string, unknown>>;
}
export interface CallArguments {
  path?: Record<string, string | number | boolean>;
  query?: Record<string, string | number | boolean | Array<string | number | boolean>>;
  headers?: Record<string, string | number | boolean>;
  body?: unknown;
  traceparent?: string;
}
export interface NulangWebClientOptions {
  baseUrl: string;
  contract: OpenApiContract;
  token?: string;
  timeoutMs?: number;
  fetchImpl?: typeof fetch;
}
export declare class ApiError extends Error {
  readonly status: number;
  readonly body: string;
  readonly headers: Headers;
  constructor(status: number, body: string, headers: Headers);
}
export declare class NulangWebClient {
  constructor(options: NulangWebClientOptions);
  call(operationId: string, args?: CallArguments): Promise<Response>;
}
