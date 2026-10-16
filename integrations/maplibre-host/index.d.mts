/** Minimal structural interface for the MapLibre GL JS functions used by this host. */
export interface MapLibreNamespace {
  Map: new (options: Record<string, unknown>) => MapLibreMap;
}
export interface MapLibreMap {
  isStyleLoaded(): boolean;
  on(event: string, cb: (event: unknown) => void): void;
  off(event: string, cb: (event: unknown) => void): void;
  getSource(id: string): MapLibreSource | undefined;
  addSource(id: string, source: SourceSpec): void;
  removeSource(id: string): void;
  getLayer(id: string): unknown;
  addLayer(layer: LayerSpec): void;
  removeLayer(id: string): void;
  setLayoutProperty(id: string, key: string, value: unknown): void;
  jumpTo(camera: Camera): void;
  setFeatureState(target: FeatureTarget, state: Record<string, unknown>): void;
  removeFeatureState(target: FeatureTarget, key?: string): void;
  remove(): void;
}
export interface MapLibreSource { setData?(data: unknown): void }
export type Camera = { center?: readonly [number, number]; zoom?: number; bearing?: number; pitch?: number };
export type SourceSpec =
  | { type: 'geojson'; data: unknown; [key: string]: unknown }
  | { type: 'vector' | 'raster' | 'raster-dem' | 'image' | 'video'; [key: string]: unknown };
export interface LayerSpec {
  id: string; type: string; source: string; 'source-layer'?: string;
  layout?: Record<string, unknown>; paint?: Record<string, unknown>;
  [key: string]: unknown;
}
export type FeatureTarget = { source: string; id: string | number; sourceLayer?: string };
export type MapCommand =
  | { type: 'camera.jump'; camera: Camera }
  | { type: 'source.upsert'; id: string; source: SourceSpec }
  | { type: 'source.remove'; id: string }
  | { type: 'layer.add'; layer: LayerSpec }
  | { type: 'layer.remove'; id: string }
  | { type: 'layer.visibility'; id: string; visible: boolean }
  | { type: 'feature-state.set'; target: FeatureTarget; state: Record<string, unknown> }
  | { type: 'feature-state.remove'; target: FeatureTarget; key?: string };
export interface MapHost {
  dispatch(command: MapCommand): void;
  on(event: string, cb: (event: unknown) => void): () => void;
  destroy(): void;
}
export interface NulangSignal<T> {
  get(): T;
  subscribe(listener: (value: T) => void): () => void;
}
export function createMapHost(options: {
  maplibregl: MapLibreNamespace; container: HTMLElement;
  style: string | Record<string, unknown>;
  center?: [number, number]; zoom?: number;
}): MapHost;
export function bindNulangSignal<T>(host: MapHost, signal: NulangSignal<T>,
  toCommand: (value: T) => MapCommand | null | undefined): () => void;
