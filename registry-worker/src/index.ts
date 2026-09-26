import { sortSemver } from './semver';

const PACKAGE_EXTENSIONS = ['.tar.zst', '.tar.gz'] as const;

function packageExtensionForRequest(request: Request): (typeof PACKAGE_EXTENSIONS)[number] {
  return request.headers.get('Content-Type')?.split(';', 1)[0].trim().toLowerCase() ===
    'application/zstd'
    ? '.tar.zst'
    : '.tar.gz';
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
      const key = `${name}/${version}.tar.gz`;

      if (method === 'GET') {
        const object = await env.BUCKET.get(key);
        if (!object) {
          return new Response('Not found', { status: 404 });
        }
        
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set('Content-Type', 'application/octet-stream');

        return new Response(object.body as ReadableStream, {
          headers
        });
      }

      if (method === 'PUT') {
        const auth = request.headers.get('Authorization');
        if (!env.PUBLISH_TOKEN || auth !== `Bearer ${env.PUBLISH_TOKEN}`) {
          return new Response('Unauthorized', { status: 401 });
        }

        // Quota hooks need a byte count, which chunked transfers (no
        // Content-Length) cannot provide pre-flight. Reject them rather than
        // reporting size_bytes: 0 and letting the quota check be bypassed.
        if (env.QUOTA_HOOK_URL && !request.headers.has('Content-Length')) {
          return new Response(
            'Length Required: Content-Length header required when quota hook is enabled',
            { status: 411 }
          );
        }

        // A package version is immutable regardless of archive compression.
        for (const extension of PACKAGE_EXTENSIONS) {
          const existing = await env.BUCKET.head(`${name}/${version}${extension}`);
          if (existing) {
            return new Response('Conflict: Version already exists', { status: 409 });
          }
        }

        // Optional publish-quota hook (hosted deployments)
        const quotaRejection = await checkPublishQuota(
          env,
          name,
          version,
          Number(request.headers.get('Content-Length') ?? 0)
        );
        if (quotaRejection) {
          return quotaRejection;
        }
        
        const extension = packageExtensionForRequest(request);
        const key = `${name}/${version}${extension}`;
        await env.BUCKET.put(key, request.body, {
          httpMetadata: {
            contentType:
              extension === '.tar.zst' ? 'application/zstd' : 'application/gzip',
          },
        });
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
