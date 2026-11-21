/** Lightweight Fetch client for Nulang Web's compiler-derived OpenAPI routes.
 * Works in browsers, Node >=18, and edge runtimes with Fetch support.
 * Not a client for Nulang's internal actor wire protocol.
 */
const HTTP_METHODS = new Set(['get', 'put', 'post', 'delete', 'patch', 'head', 'options', 'trace']);
const TRACEPARENT = /^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$/;

export class ApiError extends Error {
  constructor(status, body, headers) {
    super('Nulang Web HTTP ' + status);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
    this.headers = headers;
  }
}

function scalar(value) {
  if (value === null || value === undefined || typeof value === 'object') {
    throw new TypeError('parameter must be a scalar (or a query list of scalars)');
  }
  return String(value);
}

function encodeForm(body, mediaType) {
  if (!mediaType || typeof mediaType !== 'object' || Array.isArray(mediaType)) throw new TypeError('form media type must be an object');
  if (!body || typeof body !== 'object' || Array.isArray(body)) throw new TypeError('form body must be an object');
  const schema = mediaType.schema;
  if (schema !== undefined) {
    if (!schema || typeof schema !== 'object' || Array.isArray(schema)) throw new TypeError('form request schema must be an object');
    const properties = schema.properties;
    if (!properties || typeof properties !== 'object' || Array.isArray(properties)) throw new TypeError('form request schema must define object properties');
    const required = schema.required ?? [];
    if (!Array.isArray(required) || required.some(name => typeof name !== 'string')) throw new TypeError('form request schema has invalid required fields');
    for (const name of required) {
      if (!Object.hasOwn(properties, name)) throw new TypeError('required form field is not declared: ' + name);
      const value = body[name];
      if (!Object.hasOwn(body, name) || value === null || value === undefined || (Array.isArray(value) && !value.length)) {
        throw new TypeError('missing required form field: ' + name);
      }
    }
    for (const name of Object.keys(body)) {
      if (!Object.hasOwn(properties, name)) throw new TypeError('undeclared form field: ' + name);
    }
  }
  const form = new URLSearchParams();
  for (const [name, value] of Object.entries(body)) {
    for (const item of (Array.isArray(value) ? value : [value])) form.append(name, scalar(item));
  }
  return form.toString();
}

export class NulangWebClient {
  constructor({baseUrl, contract, token, timeoutMs = 10000, fetchImpl = globalThis.fetch}) {
    const url = new URL(baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.search || url.hash) {
      throw new TypeError('baseUrl must be an HTTP(S) URL without query or fragment');
    }
    if (!(timeoutMs > 0) || typeof fetchImpl !== 'function') throw new TypeError('invalid timeout or fetch implementation');
    if (!contract || !String(contract.openapi || '').startsWith('3.') || !contract.paths || typeof contract.paths !== 'object') {
      throw new TypeError('an OpenAPI 3.x document with paths is required');
    }
    this.base = url;
    this.token = token;
    this.timeoutMs = timeoutMs;
    this.fetch = fetchImpl;
    this.operations = new Map();
    for (const [route, methods] of Object.entries(contract.paths)) {
      if (!route.startsWith('/') || route.startsWith('//') || route.includes('?') || route.includes('#')) throw new TypeError('invalid contract path: ' + route);
      for (const [method, op] of Object.entries(methods)) {
        if (!HTTP_METHODS.has(method)) continue;
        if (!op || typeof op.operationId !== 'string' || !op.operationId) throw new TypeError('missing operationId: ' + method + ' ' + route);
        if (op['x-nulang-binding-diagnostics']?.length) throw new TypeError('invalid Nulang bindings: ' + method + ' ' + route);
        if (this.operations.has(op.operationId)) throw new TypeError('duplicate operationId: ' + op.operationId);
        this.operations.set(op.operationId, {method:method.toUpperCase(), route, op});
      }
    }
  }

  async call(operationId, {path = {}, query = {}, headers = {}, body, traceparent} = {}) {
    const meta = this.operations.get(operationId);
    if (!meta) throw new TypeError('Unknown operationId: ' + operationId);
    const {method, op} = meta;
    let {route} = meta;
    const values = {path, query, header:headers};
    const declared = {path:new Set(), query:new Set(), header:new Set()};
    for (const p of (op.parameters || [])) {
      if (p.in === 'cookie') throw new TypeError('cookie parameters are not supported by this cross-platform client');
      if (!(p.in in values) || typeof p.name !== 'string') throw new TypeError('unsupported or malformed OpenAPI parameter');
      declared[p.in].add(p.name);
      if (p.required && (values[p.in][p.name] === null || values[p.in][p.name] === undefined)) {
        throw new TypeError('missing required ' + p.in + ' parameter: ' + p.name);
      }
    }
    for (const key of Object.keys(values)) for (const name of Object.keys(values[key])) {
      if (!declared[key].has(name)) throw new TypeError('undeclared ' + key + ' parameter: ' + name);
    }
    route = route.replace(/\{([^{}]+)\}/g, (_, name) => {
      if (!(name in path)) throw new TypeError('missing path parameter: ' + name);
      const value = scalar(path[name]);
      if (value === '.' || value === '..') throw new TypeError('path parameter cannot be a dot segment');
      return encodeURIComponent(value);
    });
    const params = new URLSearchParams();
    for (const [name, value] of Object.entries(query)) {
      for (const item of (Array.isArray(value) ? value : [value])) params.append(name, scalar(item));
    }
    const reqHeaders = {...headers};
    for (const [name, value] of Object.entries(reqHeaders)) {
      reqHeaders[name] = scalar(value);
      if (/[\r\n]/.test(name) || /[\r\n]/.test(reqHeaders[name])) throw new TypeError('HTTP headers cannot contain CR or LF');
    }
    if (this.token !== undefined) {
      if (Object.keys(reqHeaders).some(name => name.toLowerCase() === 'authorization')) {
        throw new TypeError('Authorization header conflicts with configured bearer token');
      }
      if (/[\r\n]/.test(this.token)) throw new TypeError('invalid bearer token');
      reqHeaders.Authorization = 'Bearer ' + this.token;
    }
    if (traceparent !== undefined) {
      if (!TRACEPARENT.test(traceparent) || /^0{32}$/.test(traceparent.slice(3, 35)) || /^0{16}$/.test(traceparent.slice(36,52))) throw new TypeError('invalid W3C traceparent');
      reqHeaders.traceparent = traceparent;
    }
    const requestBody = op.requestBody || {};
    let encodedBody;
    if (body !== undefined && body !== null) {
      const content = requestBody.content || {};
      if ('application/x-www-form-urlencoded' in content) {
        encodedBody = encodeForm(body, content['application/x-www-form-urlencoded']);
        reqHeaders['Content-Type'] = 'application/x-www-form-urlencoded';
      } else if ('application/json' in content) {
        encodedBody = JSON.stringify(body);
        reqHeaders['Content-Type'] = 'application/json';
      } else throw new TypeError('request body media type is not declared in OpenAPI');
    } else if (requestBody.required) throw new TypeError('missing required request body');
    const url = new URL(this.base.href);
    url.pathname = this.base.pathname.replace(/\/$/, '') + route;
    url.search = params.toString();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const response = await this.fetch(url.href, {method, headers:reqHeaders, body:encodedBody, signal:controller.signal, redirect:'manual'});
      if (!response.ok) throw new ApiError(response.status, await response.text(), response.headers);
      return response;
    } finally { clearTimeout(timer); }
  }
}
