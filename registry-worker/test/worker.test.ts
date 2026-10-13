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
      headers: {
        Authorization: 'Bearer secret',
        'X-Nula-Signature': 'ab'.repeat(64),
        ...extraHeaders,
      },
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
  it('rejects unbounded chunked archive uploads even without billing', async () => {
    const env = makeEnv();
    const res = await publish(env, chunkedBody('tarball-bytes'));
    expect(res.status).toBe(411);
    expect(env.stored).toHaveLength(0);
  });

  it('rejects declared compressed archives over 64 MiB before storage', async () => {
    const env = makeEnv();
    const res = await publish(env, 'tiny-body', { 'Content-Length': String(64 * 1024 * 1024 + 1) });
    expect(res.status).toBe(413);
    expect(env.stored).toHaveLength(0);
  });

  it('rejects malformed upload lengths before storage', async () => {
    const env = makeEnv();
    const res = await publish(env, 'tiny-body', { 'Content-Length': 'NaN' });
    expect(res.status).toBe(400);
    expect(env.stored).toHaveLength(0);
  });

  it('accepts bounded uploads without a billing hook', async () => {
    const env = makeEnv();
    const res = await publish(env, 'tarball-bytes', { 'Content-Length': '13' });
    expect(res.status).toBe(201);
    expect(env.stored).toEqual(['foo/1.0.0.tar.gz']);
  });
});

describe('publisher signature transport', () => {
  it('rejects an unsigned package even with valid publish authorization', async () => {
    const env = makeEnv();
    const res = await publish(env, 'tarball-bytes', {
      'Content-Length': '13',
      'X-Nula-Signature': '',
    });
    expect(res.status).toBe(400);
    expect(env.stored).toHaveLength(0);
  });

  it('rejects a malformed detached Ed25519 signature', async () => {
    const env = makeEnv();
    const res = await publish(env, 'tarball-bytes', {
      'Content-Length': '13',
      'X-Nula-Signature': 'not-a-signature',
    });
    expect(res.status).toBe(400);
    expect(env.stored).toHaveLength(0);
  });

  it('persists signature as immutable object metadata on publish', async () => {
    const stored: Array<{ key: string; signature: string }> = [];
    const env = makeEnv({
      BUCKET: {
        head: async () => null,
        put: async (key: string, _body: unknown, options: { customMetadata: Record<string, string> }) => {
          stored.push({ key, signature: options.customMetadata.nulaSignature });
        },
      } as unknown as Env['BUCKET'],
    });
    const res = await publish(env, 'tarball-bytes', { 'Content-Length': '13' });
    expect(res.status).toBe(201);
    expect(stored).toEqual([{
      key: 'foo/1.0.0.tar.gz',
      signature: 'ab'.repeat(64),
    }]);
  });
});

describe('signed package download', () => {
  function envWithObject(signature?: string): Env {
    return makeEnv({
      BUCKET: {
        get: async () => ({
          body: new Blob(['tarball-bytes']).stream(),
          customMetadata: signature === undefined ? {} : { nulaSignature: signature },
          writeHttpMetadata: (_headers: Headers) => {},
        }),
      } as unknown as Env['BUCKET'],
    });
  }

  it('returns the publisher signature with the exact archive bytes', async () => {
    const signature = 'cd'.repeat(64);
    const response = await worker.fetch(
      new Request('http://localhost/api/v1/packages/foo/1.0.0'),
      envWithObject(signature),
      {} as ExecutionContext,
    );
    expect(response.status).toBe(200);
    expect(response.headers.get('X-Nula-Signature')).toBe(signature);
    expect(await response.text()).toBe('tarball-bytes');
  });

  it('does not serve a legacy unsigned registry object', async () => {
    const response = await worker.fetch(
      new Request('http://localhost/api/v1/packages/foo/1.0.0'),
      envWithObject(),
      {} as ExecutionContext,
    );
    expect(response.status).toBe(409);
  });
});
