import type { MapHost, MapViewport } from './index.mjs';

export interface ViewportRequest extends MapViewport {
  signal: AbortSignal;
}
export interface GeoJsonFeatureCollection {
  type: 'FeatureCollection';
  features: unknown[];
  [key: string]: unknown;
}
export interface ViewportQueryOptions {
  host: MapHost;
  sourceId: string;
  fetchFeatures(request: ViewportRequest): Promise<GeoJsonFeatureCollection>;
  debounceMs?: number;
  maxFeatures?: number;
  onError?(error: unknown): void;
}
export type DisposeViewportQuery = (() => void) & { refresh(): void };
/**
 * Query settled map viewports, cancel stale fetches, and update a retained source.
 * Expects a dedicated source ID that is not shared with another observer.
 */
export function observeViewportGeoJson(options: ViewportQueryOptions): DisposeViewportQuery;
