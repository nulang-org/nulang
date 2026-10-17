#!/usr/bin/env node
// Minimal dependency-free TypeScript request client from compiler-emitted Web IR v1.
// Deliberately returns Response: IR v1 does not define response media types.
import { readFile, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const identifier = /^[A-Za-z_$][\w$]*$/;
const types = { String: 'string', Int: 'number', Float: 'number', Bool: 'boolean' };
const reservedOperations = new Set(['constructor', 'baseUrl', 'transport', 'then', '__proto__', 'prototype']);

function tsType(type, label) {
  if (type == null) return 'string'; // Current runtime decodes untyped values as strings.
  if (!Object.hasOwn(types, type)) throw new Error(`unsupported Nulang type '${type}' at ${label}`);
  return types[type];
}

function nameFor(binding) {
  const name = identifier.test(binding.source_name) ? binding.source_name : binding.handler_param;
  if (!identifier.test(name) || name === 'constructor') {
    throw new Error(`unsupported parameter name '${binding.source_name}'`);
  }
  return name;
}

function renderRoute(route) {
  const method = route.method?.toUpperCase();
  const context = `${method} ${route.path}`;
  if (!['GET', 'POST'].includes(method)) throw new Error(`unsupported method at ${context}`);
  const name = route.handler;
  if (!identifier.test(name ?? '') || reservedOperations.has(name)) {
    throw new Error(`unsupported handler name '${name}' at ${context}`);
  }
  if (typeof route.path !== 'string' || !route.path.startsWith('/')) {
    throw new Error(`invalid absolute path at ${context}`);
  }
  // WHATWG URL resolution treats //host and backslashes as authority changes;
  // query/fragment syntax is not part of a route path contract.
  if (route.path.startsWith('//') || /[\\?#]/.test(route.path) || /\s/.test(route.path.replace(/\{[^}]*\}/g, ''))) {
    throw new Error(`unsafe URL path at ${context}`);
  }
  const bindings = route.bindings ?? [];
  if (!Array.isArray(bindings)) throw new Error(`invalid bindings at ${context}`);
  const args = [];
  const usedNames = new Set();
  const seenSources = new Set();
  for (const binding of bindings) {
    if (!['path', 'query', 'header', 'form'].includes(binding.source)) {
      throw new Error(`unsupported request source '${binding.source}' at ${context}`);
    }
    if (binding.source === 'form' && method !== 'POST') {
      throw new Error(`unsupported form on ${context}`);
    }
    const name = nameFor(binding);
    if (usedNames.has(name)) throw new Error(`duplicate parameter name '${name}' at ${context}`);
    const sourceKey = `${binding.source}:${binding.source_name}`;
    if (seenSources.has(sourceKey)) throw new Error(`duplicate source '${sourceKey}' at ${context}`);
    usedNames.add(name);
    seenSources.add(sourceKey);
    args.push({ source: binding.source, sourceName: binding.source_name, name, type: tsType(binding.ty, context) });
  }

  const captures = new Set();
  const segments = route.path.split('/').map(segment => {
    const match = /^:([A-Za-z_$][\w$]*)$/.exec(segment) || /^\{([A-Za-z_$][\w$]*)(?:\s*:\s*[^{}]+)?\}$/.exec(segment);
    if (!match) {
      if (segment.includes('{') || segment.includes('}')) throw new Error(`invalid path template at ${context}`);
      return segment.replaceAll('\\', '\\\\').replaceAll('`', '\\`').replaceAll('${', '\\${');
    }
    const capture = match[1];
    captures.add(capture);
    let input = args.find(x => x.source === 'path' && x.sourceName === capture);
    if (!input) {
      // Legacy ambient Web.param routes have no direct binding, but the IR
      // still records path-param source types. Use those only if present.
      const param = route.params?.find(p => p.name === capture);
      if (!param) throw new Error(`missing path contract for '${capture}' at ${context}`);
      if (usedNames.has(capture)) throw new Error(`duplicate parameter name '${capture}' at ${context}`);
      input = { source: 'path', sourceName: capture, name: capture, type: tsType(param.ty, context) };
      usedNames.add(capture);
      args.push(input);
    }
    return `\${encodeURIComponent(String(params.${input.name}))}`;
  });
  for (const arg of args.filter(x => x.source === 'path')) {
    if (!captures.has(arg.sourceName)) throw new Error(`unknown path parameter '${arg.sourceName}' at ${context}`);
  }

  const signature = args.length ? `params: { ${args.map(x => `${x.name}: ${x.type}`).join('; ')} }` : '';
  const lines = [
    `  async ${name}(${signature}): Promise<Response> {`,
    `    const url = new URL(\`${segments.join('/')}\`, this.baseUrl);`,
  ];
  for (const arg of args.filter(x => x.source === 'query')) {
    lines.push(`    url.searchParams.set(${JSON.stringify(arg.sourceName)}, String(params.${arg.name}));`);
  }
  lines.push('    const headers = new Headers();');
  for (const arg of args.filter(x => x.source === 'header')) {
    lines.push(`    headers.set(${JSON.stringify(arg.sourceName)}, String(params.${arg.name}));`);
  }
  const form = args.filter(x => x.source === 'form');
  if (form.length) {
    lines.push('    const form = new URLSearchParams();');
    for (const arg of form) lines.push(`    form.set(${JSON.stringify(arg.sourceName)}, String(params.${arg.name}));`);
    lines.push('    headers.set("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8");');
  }
  lines.push(`    const response = await this.transport(url, { method: ${JSON.stringify(method)}, headers${form.length ? ', body: form' : ''} });`);
  lines.push('    if (!response.ok) throw new NulangApiError(response.status, await response.text());');
  lines.push('    return response;', '  }');
  return { name, source: lines.join('\n') };
}

export function generateClient(ir) {
  if (ir?.version !== 1 || !Array.isArray(ir.routes)) throw new Error('unsupported deployment IR version or routes');
  const methods = ir.routes.map(renderRoute);
  const seen = new Set();
  for (const method of methods) {
    if (seen.has(method.name)) throw new Error(`duplicate operation '${method.name}'`);
    seen.add(method.name);
  }
  return [
    '// Generated from Nulang Web deployment IR v1. Do not edit by hand.',
    '// Response is deliberately unparsed: IR v1 does not specify a response media type.',
    'export class NulangApiError extends Error {',
    '  constructor(public readonly status: number, public readonly body: string) {',
    '    super(`HTTP ${status}: ${body}`);',
    '    this.name = "NulangApiError";',
    '  }',
    '}', 
    'export class NulangClient {',
    '  private readonly baseUrl: string;',
    '  private readonly transport: typeof fetch;',
    '  constructor(baseUrl: string, transport: typeof fetch = fetch) {',
    '    this.baseUrl = baseUrl;',
    '    this.transport = transport;',
    '  }',
    ...methods.map(method => method.source),
    '}',
    '',
  ].join('\n');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [input, output] = process.argv.slice(2);
  if (!input || !output) {
    console.error('Usage: node scripts/nulang_ts_client.mjs <dist/nulang-app.ir.json> <client.ts>');
    process.exitCode = 2;
  } else {
    try {
      const ir = JSON.parse(await readFile(input, 'utf8'));
      await writeFile(output, generateClient(ir));
    } catch (error) {
      console.error(`Client generation failed: ${error.message}`);
      process.exitCode = 1;
    }
  }
}
