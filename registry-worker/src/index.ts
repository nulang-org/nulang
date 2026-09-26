import { sortSemver } from './semver';

const PACKAGE_EXTENSIONS = ['.tar.zst', '.tar.gz'] as const;
const MAX_PACKAGE_ARCHIVE_BYTES = 64 * 1024 * 1024;
const VERSION_LOCK_SUFFIX = '.publish.lock';

type PackageCompression = 'gzip' | 'zstd';

function packageCompressionForRequest(request: Request): PackageCompression | null {
  const contentType = request.headers
    .get('Content-Type')
    ?.split(';', 1)[0]
    .trim()
    .toLowerCase();

  if (contentType === undefined || contentType === 'application/gzip') {
    return 'gzip';
  }
  if (contentType === 'application/zstd') {
    return 'zstd';
  }
  return null;
}

function packageExtension(compression: PackageCompression): (typeof PACKAGE_EXTENSIONS)[number] {
  return compression === 'zstd' ? '.tar.zst' : '.tar.gz';
}

function packageContentType(compression: PackageCompression): string {
  return compression === 'zstd' ? 'application/zstd' : 'application/gzip';
}

function archiveMagicMatches(prefix: number[], compression: PackageCompression): boolean {
  if (compression === 'gzip') {
    return prefix.length >= 2 && prefix[0] === 0x1f && prefix[1] === 0x8b;
  }
  return (
    prefix.length >= 4 &&
    prefix[0] === 0x28 &&
    prefix[1] === 0xb5 &&
    prefix[2] === 0x2f &&
    prefix[3] === 0xfd
  );
}

function validatedArchiveStream(
  body: ReadableStream<Uint8Array>,
  compression: PackageCompression,
  state: { failure: 'too-large' | 'magic' | null }
): ReadableStream<Uint8Array> {
  const requiredPrefix = compression === 'zstd' ? 4 : 2;
  const prefix: number[] = [];
  let seen = 0;
  let validated = false;

  return body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        seen += chunk.byteLength;
        if (seen > MAX_PACKAGE_ARCHIVE_BYTES) {
          state.failure = 'too-large';
          throw new Error('package archive exceeds size limit');
        }

        if (!validated) {
          for (let index = 0; index < chunk.length && prefix.length < requiredPrefix; index++) {
            prefix.push(chunk[index]);
          }
          if (prefix.length >= requiredPrefix) {
            if (!archiveMagicMatches(prefix, compression)) {
              state.failure = 'magic';
              throw new Error('package archive content type does not match payload');
            }
            validated = true;
          }
        }

        controller.enqueue(chunk);
      },
      flush() {
        if (!validated) {
          state.failure = 'magic';
          throw new Error('package archive is truncated or has invalid magic');
        }
      },
    })
  );
}

function stripPackageExtension(filename: string): string | null {
  for (const extension of PACKAGE_EXTENSIONS) {
    if (filename.endsWith(extension)) {
      return filename.slice(0, -extension.length);
    }
  }
  return null;
}


export interface Env {
  BUCKET: R2Bucket;
  PUBLISH_TOKEN: string;
  /**
   * Optional publish-quota hook. When set, the worker POSTs
   * `{ name, version, size_bytes }` as JSON to this URL before accepting a
   * publish. A 2xx response allows the publish; any other status rejects it
   * with 402 and the hook's response body as the error message.
   * Used by the NLC hosted deployment to enforce per-tenant package quotas
   * (e.g. pointed at an nlc-billing or registry-gateway endpoint).
   *
   * Chunked transfers (no Content-Length) are rejected with 411 when this
   * hook is configured, so `size_bytes` is always the real byte count.
   */
  QUOTA_HOOK_URL?: string;
}

async function checkPublishQuota(
  env: Env,
  name: string,
  version: string,
  sizeBytes: number
): Promise<Response | null> {
  if (!env.QUOTA_HOOK_URL) {
    return null; // hook disabled — allow
  }
  try {
    const resp = await fetch(env.QUOTA_HOOK_URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, version, size_bytes: sizeBytes }),
    });
    if (resp.ok) {
      return null; // quota OK — allow
    }
    const message = await resp.text();
    return new Response(`Payment Required: ${message}`, { status: 402 });
  } catch {
    // Fail closed: if the quota hook is configured but unreachable, reject.
    return new Response('Service Unavailable: quota check failed', { status: 503 });
  }
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    // Reject bad path characters to prevent directory traversal
    if (path.includes('..')) {
      return new Response('Bad Request', { status: 400 });
    }

    // Pattern: /api/v1/packages (list all packages with their versions)
    if (path === '/api/v1/packages' && method === 'GET') {
      const packages = new Map<string, string[]>();
      let cursor: string | undefined = undefined;
      do {
        const listed: R2Objects = await env.BUCKET.list({ cursor });
        for (const obj of listed.objects) {
          // key format: "name/version.tar.{zst,gz}"
          const slash = obj.key.lastIndexOf('/');
          if (slash <= 0) continue;
          const name = obj.key.substring(0, slash);
          const version = stripPackageExtension(obj.key.substring(slash + 1));
          if (!version) continue;
          const versions = packages.get(name);
          if (versions) {
            if (!versions.includes(version)) versions.push(version);
          } else {
            packages.set(name, [version]);
          }
        }
        cursor = listed.truncated ? listed.cursor : undefined;
      } while (cursor);

      const result = Array.from(packages.entries()).map(([name, versions]) => ({
        name,
        versions: sortSemver(versions),
      }));
      return new Response(JSON.stringify({ packages: result }), {
        headers: { 'Content-Type': 'application/json' },
      });
    }

    // Pattern: /api/v1/packages/:name/:version
    const matchVersion = path.match(/^\/api\/v1\/packages\/([^\/]+)\/([^\/]+)$/);
    if (matchVersion) {
      const name = matchVersion[1];
      const version = matchVersion[2];
      if (method === 'GET') {
        let object: R2ObjectBody | null = null;
        let foundExtension: (typeof PACKAGE_EXTENSIONS)[number] | null = null;
        for (const extension of PACKAGE_EXTENSIONS) {
          object = await env.BUCKET.get(`${name}/${version}${extension}`);
          if (object) {
            foundExtension = extension;
            break;
          }
        }
        if (!object || !foundExtension) {
          return new Response('Not found', { status: 404 });
        }

        const headers = new Headers();
        object.writeHttpMetadata(headers);
        if (!headers.has('Content-Type')) {
          headers.set(
            'Content-Type',
            foundExtension === '.tar.zst' ? 'application/zstd' : 'application/gzip'
          );
        }

        return new Response(object.body as ReadableStream, {
          headers,
        });
      }

      if (method === 'PUT') {
        const auth = request.headers.get('Authorization');
        if (!env.PUBLISH_TOKEN || auth !== `Bearer ${env.PUBLISH_TOKEN}`) {
          return new Response('Unauthorized', { status: 401 });
        }

        const contentLengthHeader = request.headers.get('Content-Length');
        if (contentLengthHeader !== null) {
          const declaredLength = Number(contentLengthHeader);
          if (!Number.isSafeInteger(declaredLength) || declaredLength < 0) {
            return new Response('Bad Request: invalid Content-Length', { status: 400 });
          }
          if (declaredLength > MAX_PACKAGE_ARCHIVE_BYTES) {
            return new Response('Payload Too Large', { status: 413 });
          }
        }

        if (env.QUOTA_HOOK_URL && contentLengthHeader === null) {
          return new Response(
            'Length Required: Content-Length header required when quota hook is enabled',
            { status: 411 }
          );
        }

        const compression = packageCompressionForRequest(request);
        if (!compression) {
          return new Response('Unsupported package archive content type', { status: 415 });
        }

        const quotaRejection = await checkPublishQuota(
          env,
          name,
          version,
          Number(contentLengthHeader ?? 0)
        );
        if (quotaRejection) {
          return quotaRejection;
        }

        const lockKey = `${name}/${version}${VERSION_LOCK_SUFFIX}`;
        const onlyIf = new Headers({ 'If-None-Match': '*' });
        const reservation = await env.BUCKET.put(lockKey, '', { onlyIf });
        if (!reservation) {
          return new Response('Conflict: Version already exists', { status: 409 });
        }

        for (const extension of PACKAGE_EXTENSIONS) {
          const existing = await env.BUCKET.head(`${name}/${version}${extension}`);
          if (existing) {
            return new Response('Conflict: Version already exists', { status: 409 });
          }
        }

        if (!request.body) {
          await env.BUCKET.delete(lockKey);
          return new Response('Unsupported package archive payload', { status: 415 });
        }

        const validation = { failure: null as 'too-large' | 'magic' | null };
        const extension = packageExtension(compression);
        const key = `${name}/${version}${extension}`;
        const body = validatedArchiveStream(request.body, compression, validation);

        try {
          const stored = await env.BUCKET.put(key, body, {
            httpMetadata: {
              contentType: packageContentType(compression),
            },
          });
          if (!stored) {
            await env.BUCKET.delete(lockKey);
            return new Response('Internal Server Error', { status: 500 });
          }
        } catch {
          await env.BUCKET.delete(lockKey);
          if (validation.failure === 'too-large') {
            return new Response('Payload Too Large', { status: 413 });
          }
          if (validation.failure === 'magic') {
            return new Response('Package archive content type does not match payload', {
              status: 415,
            });
          }
          return new Response('Internal Server Error', { status: 500 });
        }

        return new Response('Created', { status: 201 });
      }
    }

    // Pattern: /api/v1/packages/:name
    const matchName = path.match(/^\/api\/v1\/packages\/([^\/]+)$/);
    if (matchName && method === 'GET') {
      const name = matchName[1];
      const prefix = `${name}/`;
      
      const listed = await env.BUCKET.list({ prefix });
      const versions = Array.from(
        new Set(
          listed.objects
            .map(obj => stripPackageExtension(obj.key.substring(prefix.length)))
            .filter((version): version is string => version !== null)
        )
      );

      if (versions.length === 0) {
        return new Response('Not found', { status: 404 });
      }

      return new Response(JSON.stringify({ name, versions: sortSemver(versions) }), {
        headers: { 'Content-Type': 'application/json' }
      });
    }

    return new Response('Not found', { status: 404 });
  }
}
