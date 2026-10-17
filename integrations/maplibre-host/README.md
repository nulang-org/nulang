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
