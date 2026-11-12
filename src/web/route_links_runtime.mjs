// Browser-only, no dependencies or side effects. Included in app.routes.mjs.
const owns = (object, key) => Object.prototype.hasOwnProperty.call(object, key);
const isRecord = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const encode = (value) => encodeURIComponent(value).replace(/[!'()*]/g, (char) =>
  '%' + char.charCodeAt(0).toString(16).toUpperCase());

function scalar(value, type, location) {
  if (type === 'Int') {
    // Match Nulang's signed 48-bit VM integer domain, not JS's larger safe range.
    if (typeof value !== 'number' || !Number.isInteger(value) ||
        value < -140737488355328 || value > 140737488355327) {
      throw new TypeError(`invalid ${location} integer`);
    }
  } else if (type === 'Float') {
    if (typeof value !== 'number' || !Number.isFinite(value)) {
      throw new TypeError(`invalid ${location} float`);
    }
  } else if (type === 'Bool') {
    if (typeof value !== 'boolean') throw new TypeError(`invalid ${location} boolean`);
  } else if (typeof value !== 'string') {
    throw new TypeError(`invalid ${location} string`);
  }
  return String(value);
}

export function urlFor(route, args = {}) {
  if (typeof route !== 'string' || !owns(routeSpecs, route)) throw new TypeError('unknown route');
  if (!isRecord(args) || Object.keys(args).some((key) => key !== 'path' && key !== 'query')) {
    throw new TypeError('invalid route arguments');
  }
  const spec = routeSpecs[route];
  const path = args.path === undefined ? {} : args.path;
  const query = args.query === undefined ? {} : args.query;
  if (!isRecord(path) || !isRecord(query)) throw new TypeError('invalid route arguments');
  const pathKeys = spec.segments.filter((segment) => segment.kind === 'param').map((s) => s.name);
  if (Object.keys(path).some((key) => !pathKeys.includes(key))) throw new TypeError('unknown path parameter');
  const pieces = spec.segments.map((segment) => {
    if (segment.kind === 'literal') return encode(segment.value);
    if (!owns(path, segment.name)) throw new TypeError('missing path parameter');
    const value = scalar(path[segment.name], segment.ty, 'path');
    if (!value || value === '.' || value === '..' || /[/\\\u0000-\u001F\u007F]/.test(value)) {
      throw new TypeError('invalid path segment');
    }
    return encode(value);
  });
  const queryPairs = [];
  for (const key of Object.keys(spec.query)) {
    if (!owns(query, key) || query[key] === undefined) {
      throw new TypeError('missing query parameter');
    }
  }
  for (const key of Object.keys(query).sort()) {
    if (!owns(spec.query, key)) throw new TypeError('unknown query parameter');
    queryPairs.push(`${encode(key)}=${encode(scalar(query[key], spec.query[key], 'query'))}`);
  }
  const pathname = '/' + pieces.join('/');
  return queryPairs.length ? `${pathname}?${queryPairs.join('&')}` : pathname;
}
