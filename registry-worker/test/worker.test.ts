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
  const objects = new Map<string, { contentType?: string; bytes: Uint8Array }>();

  const bucket = {
    head: async (key: string) =>
      objects.has(key) ? ({ key } as unknown as R2Object) : null,
    get: async (key: string) => {
      const object = objects.get(key);
      if (!object) return null;
      return {
        body: new ReadableStream({
          start(controller) {
            controller.enqueue(object.bytes);
            controller.close();
          },
        }),
        writeHttpMetadata(headers: Headers) {
          if (object.contentType) headers.set('Content-Type', object.contentType);
        },
      } as unknown as R2ObjectBody;
    },
    put: async (
      key: string,
      value: ReadableStream | ArrayBuffer | ArrayBufferView | string | null | Blob,
      options?: R2PutOptions
    ) => {
      const onlyIf = options?.onlyIf;
      const ifNoneMatch =
        onlyIf instanceof Headers ? onlyIf.get('If-None-Match') : undefined;
      if (ifNoneMatch === '*' && objects.has(key)) return null;

      let bytes = new Uint8Array();
      if (typeof value === 'string') {
        bytes = new TextEncoder().encode(value);
      } else if (value instanceof Uint8Array) {
        bytes = value;
      } else if (value instanceof ArrayBuffer) {
        bytes = new Uint8Array(value);
      } else if (value instanceof ReadableStream) {
        bytes = new Uint8Array(await new Response(value).arrayBuffer());
      }
      const metadata = options?.httpMetadata;
      const contentType =
        metadata instanceof Headers
          ? metadata.get('Content-Type') ?? undefined
          : metadata?.contentType;
      objects.set(key, { contentType, bytes });
      if (key.endsWith('.tar.gz') || key.endsWith('.tar.zst')) {
        stored.push(key);
      }
      return { key } as unknown as R2Object;
    },
    delete: async (key: string) => {
      objects.delete(key);
    },
  } as unknown as Env['BUCKET'];

  return {
    BUCKET: bucket,
    PUBLISH_TOKEN: 'secret',
    ...overrides,
    stored,
  };
}


const GZIP_BYTES = new Uint8Array([0x1f, 0x8b, 0x08, 0x00, 0x00]);
const ZSTD_BYTES = new Uint8Array([0x28, 0xb5, 0x2f, 0xfd, 0x00]);

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

  it('stores zstd package archives under the canonical .tar.zst key', async () => {
    const env = makeEnv();
    const res = await publish(env, ZSTD_BYTES, {
      'Content-Type': 'application/zstd',
    });
    expect(res.status).toBe(201);
    expect(env.stored).toEqual(['foo/1.0.0.tar.zst']);
  });

  it('rejects a content type that disagrees with archive magic', async () => {
    const env = makeEnv();
    const res = await publish(env, GZIP_BYTES, {
      'Content-Type': 'application/zstd',
    });
    expect(res.status).toBe(415);
    expect(env.stored).toHaveLength(0);
  });

  it('rejects unsupported package archive content types', async () => {
    const env = makeEnv();
    const res = await publish(env, GZIP_BYTES, {
      'Content-Type': 'application/octet-stream',
    });
    expect(res.status).toBe(415);
    expect(env.stored).toHaveLength(0);
  });

  it('reserves a version atomically across gzip and zstd publishes', async () => {
    const env = makeEnv();
    const [gzip, zstd] = await Promise.all([
      publish(env, GZIP_BYTES, { 'Content-Type': 'application/gzip' }),
      publish(env, ZSTD_BYTES, { 'Content-Type': 'application/zstd' }),
    ]);

    expect([gzip.status, zstd.status].sort()).toEqual([201, 409]);
    expect(env.stored).toHaveLength(1);
  });

  it('preserves the stored archive content type on GET', async () => {
    const env = makeEnv();
    const published = await publish(env, ZSTD_BYTES, {
      'Content-Type': 'application/zstd',
    });
    expect(published.status).toBe(201);

    const response = await worker.fetch(
      new Request('http://localhost/api/v1/packages/foo/1.0.0'),
      env,
      {} as ExecutionContext
    );
    expect(response.status).toBe(200);
    expect(response.headers.get('Content-Type')).toBe('application/zstd');
  });
});
