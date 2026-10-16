# Retained MapLibre browser host (experimental)

This **browser adapter**, not a new GIS renderer or a Nulang language feature, owns a retained MapLibre map, commands, event subscriptions, and cleanup. The caller bundles MapLibre GL JS and its CSS, and supplies the map instance's container. No MapLibre dependency or new language syntax is added to core.

## Application integration

```js
import maplibregl from 'maplibre-gl';
import { createMapHost, bindNulangSignal } from './index.mjs';

const host = createMapHost({
  maplibregl,
  container: document.getElementById('map'),
  style: '/styles/basic.json',
  center: [-47.9, -15.8], zoom: 10,
});
host.dispatch({ type: 'source.upsert', id: 'sites', source: {
  type: 'geojson', data: { type: 'FeatureCollection', features: [] },
}});
host.dispatch({ type: 'layer.add', layer: {
  id: 'sites-layer', type: 'circle', source: 'sites',
}});

// Only available after Nulang's generated app.client.js has hydrated.
const unsubscribe = bindNulangSignal(
  host, window.nulang.signal('showSites'),
  value => ({ type: 'layer.visibility', id: 'sites-layer',
    visible: value === true || value === 'true' }),
);
// During navigation/unmount: unsubscribe(); host.destroy();
```

The page must include the Nulang client runtime and contain a hydrated `showSites` signal. The experimental runtime's initial signal value is text; applications must convert text to their intended types. This adapter doesn't inject `app.client.js`, load MapLibre itself, or automatically bind browser events to Nulang server actions.

- `host.dispatch()` retains source and layer declarations, reconciles them following MapLibre style reloads, and applies GeoJSON `setData` without remounting.
- `host.on(event, callback)` returns an unsubscribe. `host.destroy()` unregisters owned MapLibre listeners and removes the map once.
- `feature-state.set/remove` require a ready style; feature IDs must be stable and supplied by the application.
- Source structural changes other than GeoJSON data replacement require explicit removal and recreation. Removing a retained source also removes its retained layers.
- Spatial API authorization and validation remain server-side responsibilities; the host does not authenticate callers or enforce tenant isolation.
- This is a limited browser integration seam, not a Nulang-native map component or a typed GIS database layer.

Test: `node --test integrations/maplibre-host/test/*.test.mjs` (Node.js 22+).

## Viewport-scoped GeoJSON (stacked follow-up)

Use the optional `./viewport.mjs` observer with a **dedicated GeoJSON source**.
Create the empty source and layer before attaching it, so the layer is visible
even before the first request resolves.

```js
import { observeViewportGeoJson } from './viewport.mjs';

host.dispatch({type:'source.upsert', id:'sites', source:{
  type:'geojson', data:{type:'FeatureCollection',features:[]}
}});
host.dispatch({type:'layer.add', layer:{id:'pins', source:'sites', type:'circle'}});

const stop = observeViewportGeoJson({
  host, sourceId:'sites', debounceMs:120, maxFeatures:5000,
  fetchFeatures:async ({bbox, zoom, signal}) => {
    const params = new URLSearchParams({bbox:bbox.join(','),zoom:String(zoom)});
    const res = await fetch('/api/sites?' + params, {signal, credentials:'same-origin'});
    if (!res.ok) throw new Error('Geospatial query failed: ' + res.status);
    return res.json();
  },
  onError:error => console.error('Map query',error),
});
// Revalidate after record edits: stop.refresh();
// On unmount: stop(); host.destroy();
```

The observer reads `host.viewport()` and issues requests only on settled
`moveend` events (not during animation frames). It cancels old requests
immediately, ignores late responses, avoids repeating identical viewports,
validates the FeatureCollection envelope and limits feature counts. The bbox
is `[west,south,east,north]` in longitude/latitude; it may cross the
antimeridian or contain unwrapped longitude values. **The server must validate
and normalize bounds, enforce tenant/row authorization and query limits**, and
choose an appropriate GeoJSON response size. This adapter does not implement
pagination, caching or a spatial index; use MVT for dense layers.

Run focused tests with
`node --test integrations/maplibre-host/test/*.test.mjs`.
The separate MapLibre browser-smoke CI workflow installs pinned MapLibre and
Playwright versions; a docs preview alone does not verify WebGL rendering.
