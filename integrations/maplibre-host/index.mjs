/**
 * Retained browser-side MapLibre host for Nulang Web applications.
 * MapLibre is injected rather than bundled with the Nulang runtime.
 */
export function createMapHost({ maplibregl, container, style, ...options }) {
  if (typeof maplibregl?.Map !== 'function') throw new TypeError('MapLibre Map constructor required');
  if (!container) throw new TypeError('Map container required');
  if (!style) throw new TypeError('Map style required');

  const map = new maplibregl.Map({ container, style, ...options });
  const sources = new Map();
  const layers = new Map();
  const visibility = new Map();
  const listeners = new Set();
  let destroyed = false;
  // isStyleLoaded() includes outstanding source/tile work. It is not a
  // reliable indicator of whether source/layer style mutations are allowed.
  let styleReady = Boolean(map.isStyleLoaded());

  function ensureAlive() {
    if (destroyed) throw new Error('Map host is destroyed');
  }
  function ready() {
    return styleReady;
  }

  // Restore host-managed declarations after MapLibre style reloads.
  function reconcile() {
    if (destroyed || !ready()) return;
    for (const [id, source] of sources) {
      if (!map.getSource(id)) map.addSource(id, source);
    }
    for (const [id, layer] of layers) {
      if (!map.getLayer(id)) map.addLayer(layer);
      if (visibility.has(id)) map.setLayoutProperty(id, 'visibility', visibility.get(id) ? 'visible' : 'none');
    }
  }

  function onStyleLoaded() {
    styleReady = true;
    reconcile();
  }
  map.on('style.load', onStyleLoaded);
  map.on('load', onStyleLoaded);

  function dispatch(command) {
    ensureAlive();
    if (!command || typeof command !== 'object') throw new TypeError('Map command must be an object');
    switch (command.type) {
      case 'camera.jump':
        map.jumpTo(command.camera);
        break;
      case 'source.upsert': {
        const { id, source } = command;
        if (typeof id !== 'string' || !id || !source || typeof source.type !== 'string') {
          throw new TypeError('Invalid map source');
        }
        const old = sources.get(id);
        if (old) {
          if (old.type !== 'geojson' || source.type !== 'geojson' ||
              JSON.stringify({ ...old, data: null }) !== JSON.stringify({ ...source, data: null })) {
            throw new Error('Cannot replace a non-GeoJSON or structurally changed source; remove it first');
          }
          sources.set(id, source);
          try {
            if (ready()) {
              const live = map.getSource(id);
              if (live) {
                if (typeof live.setData !== 'function') throw new Error('GeoJSON source does not support setData');
                live.setData(source.data);
              } else reconcile();
            }
          } catch (error) {
            sources.set(id, old);
            throw error;
          }
        } else {
          if (ready() && map.getSource(id)) throw new Error(`Source ID already exists in basemap: ${id}`);
          sources.set(id, source);
          try { reconcile(); } catch (error) { sources.delete(id); throw error; }
        }
        break;
      }
      case 'source.remove': {
        const id = command.id;
        if (!sources.has(id)) break; // Never remove a basemap-owned source.
        for (const [layerId, layer] of layers) {
          if (layer.source === id) {
            if (ready() && map.getLayer(layerId)) map.removeLayer(layerId);
            layers.delete(layerId);
            visibility.delete(layerId);
          }
        }
        if (ready() && map.getSource(id)) map.removeSource(id);
        sources.delete(id);
        break;
      }
      case 'layer.add': {
        const layer = command.layer;
        if (!layer || typeof layer.id !== 'string' || !layer.id || !layer.type || !layer.source) {
          throw new TypeError('Invalid map layer');
        }
        if (layers.has(layer.id)) throw new Error(`Layer already declared: ${layer.id}`);
        if (ready() && map.getLayer(layer.id)) throw new Error(`Layer ID already exists in basemap: ${layer.id}`);
        layers.set(layer.id, layer);
        try { reconcile(); } catch (error) { layers.delete(layer.id); throw error; }
        break;
      }
      case 'layer.remove':
        if (layers.has(command.id) && ready() && map.getLayer(command.id)) map.removeLayer(command.id);
        layers.delete(command.id);
        visibility.delete(command.id);
        break;
      case 'layer.visibility':
        if (typeof command.visible !== 'boolean') throw new TypeError('visible must be a boolean');
        visibility.set(command.id, command.visible);
        if (layers.has(command.id) && ready() && map.getLayer(command.id)) {
          map.setLayoutProperty(command.id, 'visibility', command.visible ? 'visible' : 'none');
        }
        break;
      case 'feature-state.set':
        if (!ready()) throw new Error('Map style is not ready for feature-state commands');
        map.setFeatureState(command.target, command.state);
        break;
      case 'feature-state.remove':
        if (!ready()) throw new Error('Map style is not ready for feature-state commands');
        map.removeFeatureState(command.target, command.key);
        break;
      default:
        throw new Error(`Unsupported map command: ${String(command.type)}`);
    }
  }

  function on(event, callback) {
    ensureAlive();
    if (typeof callback !== 'function') throw new TypeError('Map event callback required');
    map.on(event, callback);
    const listener = { event, callback };
    listeners.add(listener);
    return () => {
      if (listeners.delete(listener) && !destroyed) map.off(event, callback);
    };
  }

  function destroy() {
    if (destroyed) return;
    destroyed = true;
    for (const { event, callback } of listeners) map.off(event, callback);
    listeners.clear();
    map.off('style.load', onStyleLoaded);
    map.off('load', onStyleLoaded);
    map.remove();
    sources.clear();
    layers.clear();
    visibility.clear();
  }
  function viewport() {
    ensureAlive();
    const bounds = map.getBounds();
    return {
      bbox: [bounds.getWest(), bounds.getSouth(), bounds.getEast(), bounds.getNorth()],
      zoom: map.getZoom(),
    };
  }
  return Object.freeze({ dispatch, on, viewport, destroy });
}

/** Bind a client Nulang signal to a typed map command; return an unsubscribe. */
export function bindNulangSignal(host, signal, toCommand) {
  if (!signal || typeof signal.get !== 'function' || typeof signal.subscribe !== 'function') {
    throw new TypeError('Nulang signal with get/subscribe required');
  }
  const apply = value => {
    const command = toCommand(value);
    if (command != null) host.dispatch(command);
  };
  apply(signal.get());
  return signal.subscribe(apply);
}
