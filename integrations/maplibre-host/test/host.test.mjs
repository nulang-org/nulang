import test from 'node:test';
import assert from 'node:assert/strict';
import { createMapHost, bindNulangSignal } from '../index.mjs';

function fixture({ loaded = true } = {}) {
  const maps = [];
  class FakeMap {
    constructor(opts) {
      this.opts = opts;
      this.sources = new Map();
      this.layers = new Map();
      this.handlers = new Map();
      this.actions = [];
      this.loaded = loaded;
      this.removed = 0;
      maps.push(this);
    }
    isStyleLoaded() { return this.loaded; }
    getSource(id) { return this.sources.get(id); }
    addSource(id, def) {
      const source = { ...def, setData: (data) => { source.data = data; this.actions.push(['setData', id, data]); } };
      this.sources.set(id, source); this.actions.push(['addSource', id]);
    }
    removeSource(id) { this.sources.delete(id); this.actions.push(['removeSource', id]); }
    getLayer(id) { return this.layers.get(id); }
    addLayer(layer) { this.layers.set(layer.id, { ...layer }); this.actions.push(['addLayer', layer.id]); }
    removeLayer(id) { this.layers.delete(id); this.actions.push(['removeLayer', id]); }
    setLayoutProperty(id, key, value) { this.actions.push(['layout', id, key, value]); }
    setFeatureState(target, state) { this.actions.push(['feature', target, state]); }
    removeFeatureState(target, key) { this.actions.push(['removeFeature', target, key]); }
    jumpTo(options) { this.actions.push(['camera', options]); }
    on(event, handler) { const s = this.handlers.get(event) ?? new Set(); s.add(handler); this.handlers.set(event, s); }
    off(event, handler) { this.handlers.get(event)?.delete(handler); }
    emit(event, payload) { for (const cb of this.handlers.get(event) ?? []) cb(payload); }
    remove() { this.removed++; }
  }
  return { maplibregl: { Map: FakeMap }, maps };
}

const point = { type: 'FeatureCollection', features: [] };

test('mounts once and patches camera without reconstructing the map', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  host.dispatch({ type: 'camera.jump', camera: { center: [-47.9, -15.8], zoom: 10 } });
  assert.equal(fake.maps.length, 1);
  assert.deepEqual(fake.maps[0].actions[0], ['camera', { center: [-47.9, -15.8], zoom: 10 }]);
  host.destroy();
  host.destroy();
  assert.equal(fake.maps[0].removed, 1);
  assert.throws(() => host.dispatch({ type: 'camera.jump', camera: {} }), /destroyed/);
});

test('queues sources before style load and applies them before layers', () => {
  const fake = fixture({ loaded: false });
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: point } });
  host.dispatch({ type: 'layer.add', layer: { id: 'pins', type: 'circle', source: 'sites' } });
  assert.equal(fake.maps[0].actions.length, 0);
  fake.maps[0].loaded = true;
  fake.maps[0].emit('style.load');
  assert.deepEqual(fake.maps[0].actions, [['addSource', 'sites'], ['addLayer', 'pins']]);
  host.destroy();
});

test('updates GeoJSON data in place and changes only requested feature state', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: point } });
  const next = { type: 'FeatureCollection', features: [{ type: 'Feature', properties: {}, geometry: { type: 'Point', coordinates: [0, 0] } }] };
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: next } });
  host.dispatch({ type: 'feature-state.set', target: { source: 'sites', id: 7 }, state: { selected: true } });
  assert.deepEqual(fake.maps[0].actions.map(x => x[0]), ['addSource', 'setData', 'feature']);
  host.destroy();
});

test('teardown unregisters listeners and dependency-aware removals', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: point } });
  host.dispatch({ type: 'layer.add', layer: { id: 'pins', type: 'circle', source: 'sites' } });
  let selected = 0;
  const dispose = host.on('click', () => selected++);
  fake.maps[0].emit('click', {});
  dispose();
  fake.maps[0].emit('click', {});
  assert.equal(selected, 1);
  host.dispatch({ type: 'source.remove', id: 'sites' });
  assert.deepEqual(fake.maps[0].actions.slice(-2), [['removeLayer', 'pins'], ['removeSource', 'sites']]);
  host.destroy();
  assert.equal(fake.maps[0].handlers.get('style.load').size, 0);
});

test('binds Nulang signals with synchronous initial state and reversible subscription', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  let value = false;
  const listeners = new Set();
  const signal = { get: () => value, subscribe: fn => { listeners.add(fn); return () => listeners.delete(fn); } };
  const dispose = bindNulangSignal(host, signal, visible => ({ type: 'layer.visibility', id: 'sites', visible }));
  host.dispatch({ type: 'source.upsert', id: 'points', source: { type: 'geojson', data: point } });
  host.dispatch({ type: 'layer.add', layer: { id: 'sites', source: 'points', type: 'circle' } });
  value = true; for (const cb of listeners) cb(value);
  assert.equal(fake.maps[0].actions.filter(x => x[0] === 'layout').at(-1)[3], 'visible');
  dispose();
  assert.equal(listeners.size, 0);
  host.destroy();
});

test('fails closed for unsupported commands and non-GeoJSON source replacement', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  assert.throws(() => host.dispatch({ type: 'unknown' }), /Unsupported/);
  host.dispatch({ type: 'source.upsert', id: 'tiles', source: { type: 'vector', tiles: ['https://a.test/{z}/{x}/{y}.pbf'] } });
  assert.throws(() => host.dispatch({ type: 'source.upsert', id: 'tiles', source: { type: 'raster', tiles: [] } }), /Cannot replace/);
  host.destroy();
});

test('recreates declared sources and layers after style reload without remounting', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  host.dispatch({ type: 'layer.visibility', id: 'pins', visible: false });
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: point } });
  host.dispatch({ type: 'layer.add', layer: { id: 'pins', source: 'sites', type: 'circle' } });
  const map = fake.maps[0];
  map.sources.clear(); map.layers.clear();
  map.emit('style.load');
  assert.equal(fake.maps.length, 1);
  assert.equal(map.actions.filter(action => action[0] === 'addSource').length, 2);
  assert.equal(map.actions.filter(action => action[0] === 'addLayer').length, 2);
  assert.deepEqual(map.actions.at(-1), ['layout', 'pins', 'visibility', 'none']);
  host.destroy();
});


test('rejects collisions with basemap-owned source and layer IDs', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  const map = fake.maps[0];
  map.sources.set('basemap', { type: 'vector' });
  map.layers.set('basemap-labels', { id: 'basemap-labels' });
  assert.throws(() => host.dispatch({ type: 'source.upsert', id: 'basemap', source: { type: 'geojson', data: point } }), /already exists/);
  assert.throws(() => host.dispatch({ type: 'layer.add', layer: { id: 'basemap-labels', type: 'circle', source: 'basemap' } }), /already exists/);
  host.destroy();
});

test('failed layer addition does not poison a later valid command', () => {
  const fake = fixture();
  const host = createMapHost({ maplibregl: fake.maplibregl, container: {}, style: 'sample-style' });
  const map = fake.maps[0];
  host.dispatch({ type: 'source.upsert', id: 'sites', source: { type: 'geojson', data: point } });
  const original = map.addLayer.bind(map);
  map.addLayer = layer => { if (layer.type === 'invalid') throw new Error('invalid layer'); else original(layer); };
  assert.throws(() => host.dispatch({ type: 'layer.add', layer: { id: 'pins', type: 'invalid', source: 'sites' } }), /invalid layer/);
  assert.doesNotThrow(() => host.dispatch({ type: 'layer.add', layer: { id: 'pins', type: 'circle', source: 'sites' } }));
  host.destroy();
});
