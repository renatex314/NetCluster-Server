/**
 * Node client for netcluster-server.
 *
 * @see https://github.com/renatex314/NetCluster-Server
 */

/** Batch size the reporter chunks at. See the README for the measured tradeoff. */
export declare const DEFAULT_MAX_BATCH: 1000;

export declare class NetClusterError extends Error {
  readonly name: 'NetClusterError';
  /** HTTP status, or 0 when the request never got a response. */
  readonly status: number;
  readonly url: string;
  /** The server's parsed JSON error body, when there was one. */
  readonly body: { error?: string; code?: string } | null;
}

export interface ClientOptions {
  /** A single server. */
  url?: string;
  /** Several replicas: writes fan out to all, reads go to one. */
  urls?: string[];
  /** Per-request timeout. Default 5000. */
  timeoutMs?: number;
  /** Retries for network errors and 5xx. Never applied to 4xx. Default 1. */
  retries?: number;
  /** Called when a write reached some replicas but not all. */
  onReplicaError?: (failures: { url: string; error: Error }[]) => void;
  /** Sent with every request. */
  headers?: Record<string, string>;
  fetch?: typeof fetch;
}

export interface CollectionConfig {
  maxZoom?: number;
  radius?: number;
  extent?: number;
  hysteresis?: number;
  /** Category labels; a label's position in this list is its category index. */
  categories?: string[];
  /** Drop a device that has not reported for this long. 0 disables expiry. */
  ttlSeconds?: number;
}

/** One position report. `cat` may be a label from the collection, or its index. */
export interface Point {
  id: string;
  lng: number;
  lat: number;
  cat?: number | string;
}

export interface PointFeature {
  type: 'Feature';
  id: string;
  properties: { id: string };
  geometry: { type: 'Point'; coordinates: [number, number] };
}

export interface ClusterFeature {
  type: 'Feature';
  properties: {
    cluster: true;
    cluster_id: number;
    point_count: number;
    point_count_abbreviated: string;
  };
  geometry: { type: 'Point'; coordinates: [number, number] };
}

export type Feature = PointFeature | ClusterFeature;

/** What the index knows about one registered device. */
export interface DeviceInfo {
  id: string;
  lng: number;
  lat: number;
  /** The category label, or null when the collection has no labels. */
  cat: string | null;
  cat_index: number;
  last_seen_ms: number;
  /** How long ago it reported. Compare against the collection's `ttl_seconds`. */
  age_ms: number;
}

export interface FeatureCollection {
  type: 'FeatureCollection';
  features: Feature[];
}

export interface ChildrenResult extends FeatureCollection {
  /** The zoom at which this cluster first splits. */
  expansion_zoom: number;
}

export interface CollectionStats {
  name: string;
  devices: number;
  max_zoom: number;
  radius: number;
  categories: string[];
  ttl_seconds: number;
  memory_bytes: number;
  grid_entries: number;
  centers_per_level: number[];
  ingested: number;
  queries: number;
  expired: number;
  uptime_ms: number;
  moves_fast_pct: number;
  /** 0 when no snapshot has been written, or persistence is off. */
  last_snapshot_ms: number;
  last_snapshot_bytes: number;
  snapshot_failures: number;
  /** Devices loaded from a snapshot at startup. */
  restored: number;
}

export interface Health {
  status: 'ok';
  collections: number;
  devices: number;
  uptime_ms: number;
  /** Whether the server was started with a data directory. */
  persistence: boolean;
}

export interface ReportResult {
  accepted: number;
  devices?: number;
}

export interface QueryOptions {
  /** [west, south, east, north]. Defaults to the whole world. */
  bbox?: [number, number, number, number];
  zoom?: number;
  /** A category label or index. Omit for no filter. */
  cat?: number | string;
}

export interface ReporterOptions {
  /** How often to flush. Default 500. */
  flushMs?: number;
  /** Points per request. Default {@link DEFAULT_MAX_BATCH}. */
  maxBatch?: number;
  /** Without this, a failed flush rejects the `flush()` promise instead. */
  onError?: (err: Error) => void;
}

export interface ReporterStats {
  queued: number;
  /** Reports replaced by a newer one for the same device before being sent. */
  coalesced: number;
  sent: number;
  requests: number;
  errors: number;
}

/** Accumulates reports and flushes them on a timer, coalescing by device id. */
export declare class Reporter {
  readonly collection: string;
  readonly stats: ReporterStats;
  /** Queue one report, replacing any earlier unflushed one for the same id. */
  report(point: Point): void;
  reportMany(points: Point[]): void;
  /** Send everything queued. Safe to call concurrently. */
  flush(): Promise<ReportResult>;
  /** Stop the timer and flush what is left. */
  close(): Promise<void>;
}

/** A collection name bound into every call. */
export interface BoundCollection {
  readonly name: string;
  create(config?: CollectionConfig): Promise<unknown>;
  drop(): Promise<unknown>;
  stats(): Promise<CollectionStats>;
  verify(): Promise<{ ok: boolean; detail?: string; violation?: string }>;
  snapshot(): Promise<{ snapshot: string; bytes: number }>;
  report(points: Point | Point[], opts?: { maxBatch?: number }): Promise<ReportResult>;
  remove(id: string): Promise<{ removed: boolean }>;
  has(id: string): Promise<boolean>;
  getDevice(id: string): Promise<DeviceInfo | null>;
  getClusters(opts?: QueryOptions): Promise<FeatureCollection>;
  getTile(z: number, x: number, y: number, opts?: { cat?: number | string; format?: 'mvt' }): Promise<Uint8Array>;
  getTile(z: number, x: number, y: number, opts: { cat?: number | string; format: 'json' }): Promise<FeatureCollection>;
  getChildren(clusterId: number): Promise<ChildrenResult>;
  getLeaves(clusterId: number, opts?: { limit?: number; offset?: number }): Promise<FeatureCollection>;
  deviceCluster(id: string, zoom?: number): Promise<Feature>;
  reporter(opts?: ReporterOptions): Reporter;
}

export declare class NetClusterClient {
  constructor(options?: ClientOptions);
  readonly urls: string[];

  health(): Promise<Health>;
  listCollections(): Promise<{ collections: CollectionStats[] }>;

  createCollection(name: string, config?: CollectionConfig): Promise<unknown>;
  dropCollection(name: string): Promise<unknown>;
  stats(name: string): Promise<CollectionStats>;
  verify(name: string): Promise<{ ok: boolean; detail?: string; violation?: string }>;
  /** Force a snapshot now. Rejects with code 'persistence_disabled' if the server has none. */
  snapshot(name: string): Promise<{ snapshot: string; bytes: number }>;

  report(name: string, points: Point | Point[], opts?: { maxBatch?: number }): Promise<ReportResult>;
  remove(name: string, id: string): Promise<{ removed: boolean }>;

  /** Is a device with this id currently registered? Throws if the collection is unknown. */
  has(name: string, id: string): Promise<boolean>;
  /** Position, category and staleness for one device, or null if not registered. */
  getDevice(name: string, id: string): Promise<DeviceInfo | null>;

  getClusters(name: string, opts?: QueryOptions): Promise<FeatureCollection>;
  getTile(name: string, z: number, x: number, y: number, opts?: { cat?: number | string; format?: 'mvt' }): Promise<Uint8Array>;
  getTile(name: string, z: number, x: number, y: number, opts: { cat?: number | string; format: 'json' }): Promise<FeatureCollection>;
  getChildren(name: string, clusterId: number): Promise<ChildrenResult>;
  getLeaves(name: string, clusterId: number, opts?: { limit?: number; offset?: number }): Promise<FeatureCollection>;
  deviceCluster(name: string, id: string, zoom?: number): Promise<Feature>;

  /**
   * A view whose reads always go to the same replica, keyed on a viewer or
   * session id. Prevents markers flickering as a viewer's polls bounce between
   * replicas that hold slightly different trees.
   */
  forViewer(key: string | number): NetClusterClient;

  /** Bind a collection name so you stop repeating it. */
  collection(name: string): BoundCollection;

  reporter(name: string, opts?: ReporterOptions): Reporter;
}

export default NetClusterClient;
