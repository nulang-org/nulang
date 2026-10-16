/**
 * Observe settled camera changes and feed a retained GeoJSON source.
 * fetchFeatures is provided by the application (including authentication).
 * The server must independently authorize, bound, and validate spatial queries.
 */
export function observeViewportGeoJson({
  host, sourceId, fetchFeatures, debounceMs = 120, maxFeatures = 10000,
  onError = error => console.error('nulang map viewport query:', error),
}) {
  if (!host || typeof host.on !== 'function' || typeof host.viewport !== 'function' || typeof host.dispatch !== 'function') {
    throw new TypeError('Map host with on/viewport/dispatch required');
  }
  if (typeof sourceId !== 'string' || !sourceId) throw new TypeError('sourceId required');
  if (typeof fetchFeatures !== 'function') throw new TypeError('fetchFeatures function required');
  if (!Number.isFinite(debounceMs) || debounceMs < 0) throw new RangeError('Invalid debounceMs');
  if (!Number.isSafeInteger(maxFeatures) || maxFeatures < 1) throw new RangeError('Invalid maxFeatures');
  if (typeof onError !== 'function') throw new TypeError('onError function required');

  let disposed = false;
  let requestVersion = 0;
  let lastKey;
  let timer;
  let controller;

  function viewport() {
    const value = host.viewport();
    const bbox = value?.bbox;
    const zoom = value?.zoom;
    if (!Array.isArray(bbox) || bbox.length !== 4 || !bbox.every(Number.isFinite) ||
        bbox[1] > bbox[3] || !Number.isFinite(zoom)) {
      throw new TypeError('Map host returned invalid viewport');
    }
    // Preserve longitude wrapping; server must handle antimeridian-crossing bounds.
    return { bbox: [...bbox], zoom };
  }

  async function load(version, view) {
    const current = new AbortController();
    controller = current;
    try {
      const data = await fetchFeatures({ ...view, signal: current.signal });
      if (disposed || version !== requestVersion || current.signal.aborted) return;
      if (!data || data.type !== 'FeatureCollection' || !Array.isArray(data.features)) {
        throw new TypeError('GeoJSON FeatureCollection required');
      }
      if (data.features.length > maxFeatures) throw new RangeError('GeoJSON feature limit exceeded');
      host.dispatch({type:'source.upsert', id:sourceId, source:{type:'geojson', data}});
    } catch (error) {
      if (!disposed && version === requestVersion && !current.signal.aborted) onError(error);
    } finally {
      if (controller === current) controller = undefined;
    }
  }

  function schedule(force = false) {
    if (disposed) return;
    const view = viewport();
    const key = JSON.stringify([view.bbox, view.zoom]);
    if (!force && key === lastKey) return;
    lastKey = key;
    requestVersion += 1;
    controller?.abort();
    clearTimeout(timer);
    const version = requestVersion;
    timer = setTimeout(() => { timer = undefined; void load(version, view); }, debounceMs);
  }

  const unsubscribe = host.on('moveend', () => schedule());
  schedule();
  function dispose() {
    if (disposed) return;
    disposed = true;
    requestVersion += 1;
    clearTimeout(timer);
    controller?.abort();
    unsubscribe();
  }
  dispose.refresh = () => schedule(true);
  return dispose;
}
