// Minimal ambient types for the Cloudflare Pages Function in
// functions/api/contact.ts. Deliberately avoids @cloudflare/workers-types,
// whose globals conflict with the DOM lib this Astro project compiles against.

interface EventContext<
  Env,
  Params extends string = string,
  Data extends Record<string, unknown> = Record<string, unknown>,
> {
  request: Request;
  env: Env;
  params: Record<Params, string>;
  data: Data;
  waitUntil(promise: Promise<unknown>): void;
  next(input?: Request | string, init?: RequestInit): Promise<Response>;
  functionPath: string;
}

type PagesFunction<
  Env = unknown,
  Params extends string = string,
  Data extends Record<string, unknown> = Record<string, unknown>,
> = (context: EventContext<Env, Params, Data>) => Response | Promise<Response>;
