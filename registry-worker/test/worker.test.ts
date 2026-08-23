import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import worker, { type Env } from '../src/index';

interface HookCall {
  name: string;
  version: string;
  size_bytes: number;
}

function chunkedBody(text: string): ReadableStream<Uint8Array> {
  // A stream body carries no Content-Length (chunked transfer encoding).
  return new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode(text));
      controller.close();
    },
  });
}

function makeEnv(overrides: Partial<Env> = {}): Env & { stored: string[] } {
  const stored: string[] = [];
  return {
    BUCKET: {
      head: async () => null,
      put: async (key: string) => {
        stored.push(key);
      },
    } as unknown as Env['BUCKET'],
    PUBLISH_TOKEN: 'secret',
    ...overrides,
    stored,
  };
}

function publish(
  env: Env,
  body: BodyInit | null,
  extraHeaders: Record<string, string> = {}
) {
  return worker.fetch(
    new Request('http://localhost/api/v1/packages/foo/1.0.0', {
      method: 'PUT',
      headers: { Authorization: 'Bearer secret', ...extraHeaders },
      body,
      duplex: 'half',
    } as RequestInit),
    env,
    {} as ExecutionContext
  );
}

describe('with QUOTA_HOOK_URL configured', () => {
  const hookUrl = 'https://billing.example/hook';
  const fetchMock = vi.fn<typeof fetch>();

  beforeAll(() => {
    vi.stubGlobal('fetch', fetchMock);
  });

  afterAll(() => {
    vi.unstubAllGlobals();
  });

  afterEach(() => {
    fetchMock.mockReset();
  });

  it('rejects chunked PUTs with 411 before the quota hook runs', async () => {
    const env = makeEnv({ QUOTA_HOOK_URL: hookUrl });
    const res = await publish(env, chunkedBody('tarball-bytes'));
    expect(res.status).toBe(411);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(env.stored).toHaveLength(0);
  });

  it('passes the real byte count to the quota hook and stores on approval', async () => {
    fetchMock.mockResolvedValue(new Response('ok', { status: 200 }));
    const env = makeEnv({ QUOTA_HOOK_URL: hookUrl });
    const payload = 'tarball-bytes';
    const res = await publish(env, payload, { 'Content-Length': String(payload.length) });
    expect(res.status).toBe(201);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe(hookUrl);
    expect(JSON.parse(String(init?.body))).toEqual({
      name: 'foo',
      version: '1.0.0',
      size_bytes: payload.length,
    });
    expect(env.stored).toEqual(['foo/1.0.0.tar.gz']);
  });

  it('returns 402 with the hook message and does not store', async () => {
    fetchMock.mockResolvedValue(new Response('quota exceeded', { status: 402 }));
    const env = makeEnv({ QUOTA_HOOK_URL: hookUrl });
    const res = await publish(env, 'tarball-bytes', { 'Content-Length': '13' });
    expect(res.status).toBe(402);
    expect(await res.text()).toBe('Payment Required: quota exceeded');
    expect(env.stored).toHaveLength(0);
  });
});

describe('without QUOTA_HOOK_URL', () => {
  it('accepts chunked PUTs when no quota hook is configured', async () => {
    const env = makeEnv();
    const res = await publish(env, chunkedBody('tarball-bytes'));
    expect(res.status).toBe(201);
    expect(env.stored).toEqual(['foo/1.0.0.tar.gz']);
  });
});
