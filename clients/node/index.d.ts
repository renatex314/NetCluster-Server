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
  /** Retries for network errors, 429 and 5xx, with backoff. Default 1. */
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
  /**
   * Category labels; a label's position in this list is its category index.
   *
   * A shorthand for one dimension named `cat`. Use `dimensions` for anything
   * more; passing both is an error.
   */
  categories?: string[];
  /**
   * Properties this collection can filter on. `multi` lets one device hold
   * several values at once -- a vehicle owned by three clients, which a single
   * category cannot express.
   */
  dimensions?: Dimension[];
  /**
   * Which combinations of dimensions a query may name. Defaults to each on its
   * own.
   *
   * This is what filtering costs: a device contributes one aggregate entry per
   * shape per tree level, so `[['client'], ['status'], ['client','status']]`
   * costs three times what `[['client']]` does. A query that does not match a
   * declared shape is a 400, never a silent scan.
   */
  filters?: string[][];
  /**
   * Property fields that `where` can search. Each costs one string per device,
   * extracted from `props` at ingest, so a scan never parses JSON. Adding one
   * later means recreating the collection.
   */
  text?: string[];
  /** Drop a device that has not reported for this long. 0 disables expiry. */
  ttlSeconds?: number;
  /**
   * Largest per-device `props` blob accepted, in bytes. Default 1024; 0 refuses
   * properties entirely.
   *
   * Memory is bounded by devices times this number, so it is a real limit: at a
   * million devices every kilobyte allowed here is a gigabyte promised.
   */
  maxPropsBytes?: number;
}

/**
 * One filterable property, and the values it can take.
 *
 * Give it `values` when you know them, or `capacity` when you do not. With
 * `capacity` the values are interned as they arrive, so the ceiling is how many
 * distinct ones have ever been assigned rather than how large an id may get — auto-increment
 * client ids running into the millions are fine behind `capacity: 4096`.
 */
export interface Dimension {
  name: string;
  /** Value labels; a label's position in this list is its value index. */
  values?: string[];
  /** How many distinct values may exist, when they are not known up front. */
  capacity?: number;
  /** May one device hold several of these at once? Default false. */
  multi?: boolean;
}

/** One position report. `cat` may be a label from the collection, or its index. */
export interface Point {
  id: string;
  lng: number;
  lat: number;
  /**
   * Strictly increasing source version. Defaults to a process-local logical clock.
   * An older retry is ignored by the server instead of moving the device back.
   */
  updatedAt?: number;
  cat?: number | string;
  /**
   * Filter values, when the collection declares `dimensions`:
   * `{ client: ['1', '7'], status: 'enroute' }`.
   *
   * Omit it and the device keeps the values it already had, exactly as omitting
   * `props` keeps its properties -- a bare position report must not re-file a
   * vehicle into whatever value happens to sit at index 0.
   */
  dims?: Record<string, string | number | Array<string | number>>;
  /**
   * Free-form attributes for this device. Any JSON object.
   *
   * Omit it and the device keeps whatever it already had -- positions arrive far
   * more often than attributes change, so a position report does not have to
   * resend the number plate to avoid erasing it. Send `{}` to clear.
   *
   * There is no partial update: this replaces the whole object, so changing one
   * field means resending it. Merge semantics on nested values are ambiguous and
   * replacement is not.
   *
   * Capped by the collection's `maxPropsBytes` (default 1024).
   */
  props?: Record<string, unknown>;
}

export interface PointFeature {
  type: 'Feature';
  id: string;
  /** The device's `props` when it has any, otherwise `{ id }`. */
  properties: Record<string, unknown>;
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
  /** Last accepted source-side version, when the client supplied one. */
  updated_at_ms: number | null;
  /** Whatever was last reported for this device, or null. */
  props: Record<string, unknown> | null;
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
  /** Reports rejected because their source version was older than the stored one. */
  stale_reports: number;
  /** Defensive index rebuilds after a detected materialized-view mismatch. */
  repairs: number;
  /** Number of currently interned dynamic values per dimension. */
  interned: number[];
  /** Declared value capacity per dimension. */
  dimension_capacities: number[];
  /** Bytes of device properties currently held. */
  props_bytes: number;
  max_props_bytes: number;
  /** Bytes held by the searchable text fields. */
  text_bytes: number;
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
  /** Reports ignored because their source version was older than the stored one. */
  stale?: number;
  devices?: number;
}

export interface QueryOptions {
  /** [west, south, east, north]. Defaults to the whole world. */
  bbox?: [number, number, number, number];
  zoom?: number;
  /** A category label or index. Omit for no filter. */
  cat?: number | string;
  /**
   * One value per dimension of a declared filter shape, e.g.
   * `{ client: 7, status: 'enroute' }`. Sent as `?f.client=7&f.status=enroute`.
   *
   * A device may hold several values for a `multi` dimension, but a query names
   * one of them. An undeclared combination or dimension is a 400. A *value* is
   * a 400 only on a dimension whose values were declared: on one with a
   * `capacity`, a value nothing has reported yet is an empty result, because the
   * caller cannot know which values exist.
   */
  filter?: Record<string, string | number>;
  /**
   * Substring search over a declared text field. `'plate~abc'`, or an object
   * where a bare value is a substring and `{ eq }` is the whole value:
   *
   * ```ts
   * { plate: 'abc' }                // plate~abc
   * { plate: { eq: 'ABC-1234' } }   // plate=ABC-1234
   * ```
   *
   * Matching ignores case. Unlike `filter`, this **scans**: a substring has
   * nothing to keep a running count of, so it costs O(devices) rather than
   * O(markers). Reach for `filter` whenever the values can be declared.
   */
  where?: string | Record<string, string | number | { eq: string | number }>;
}

/** The filter half of {@link QueryOptions}, for tiles. */
export interface TileOptions {
  cat?: number | string;
  filter?: Record<string, string | number>;
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
  /** Reports ignored because their source version was older than the stored one. */
  stale: number;
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

/**
 * A GeoJSON Feature accepted on ingest.
 *
 * Looser than what queries return: the id may sit on the feature (where GeoJSON
 * says it goes) or in a property, `properties` may be null, and a third
 * coordinate is allowed and ignored. The geometry must be a Point.
 */
export interface InputFeature {
  type: 'Feature';
  /** Source version; sent as a top-level GeoJSON foreign member. */
  updatedAt?: number;
  updated_at_ms?: number;
  id?: string | number;
  properties: Record<string, unknown> | null;
  geometry: { type: 'Point'; coordinates: number[] };
}

export interface InputFeatureCollection {
  type: 'FeatureCollection';
  features: InputFeature[];
}

export interface ReportGeoJSONOptions {
  maxBatch?: number;
  /**
   * Which property holds the id, when a Feature has no `id` of its own. Naming
   * it is strict: a Feature missing that property is rejected rather than
   * falling back to `feature.id`.
   */
  idProperty?: string;
  /** Which property holds the category. Defaults to `cat`, then `category`. */
  catProperty?: string;
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
  /** Report positions as GeoJSON. Same endpoint and upsert semantics as `report`. */
  reportGeoJSON(
    geojson: InputFeatureCollection | InputFeature[] | InputFeature,
    opts?: ReportGeoJSONOptions,
  ): Promise<ReportResult>;
  remove(id: string): Promise<{ removed: boolean }>;
  has(id: string): Promise<boolean>;
  getDevice(id: string): Promise<DeviceInfo | null>;
  getClusters(opts?: QueryOptions): Promise<FeatureCollection>;
  getTile(z: number, x: number, y: number, opts?: TileOptions & { format?: 'mvt' }): Promise<Uint8Array>;
  getTile(z: number, x: number, y: number, opts: TileOptions & { format: 'json' }): Promise<FeatureCollection>;
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
  /** Report positions as GeoJSON. Same endpoint and upsert semantics as `report`. */
  reportGeoJSON(
    name: string,
    geojson: InputFeatureCollection | InputFeature[] | InputFeature,
    opts?: ReportGeoJSONOptions,
  ): Promise<ReportResult>;
  remove(name: string, id: string): Promise<{ removed: boolean }>;

  /** Is a device with this id currently registered? Throws if the collection is unknown. */
  has(name: string, id: string): Promise<boolean>;
  /** Position, category and staleness for one device, or null if not registered. */
  getDevice(name: string, id: string): Promise<DeviceInfo | null>;

  getClusters(name: string, opts?: QueryOptions): Promise<FeatureCollection>;
  getTile(name: string, z: number, x: number, y: number, opts?: TileOptions & { format?: 'mvt' }): Promise<Uint8Array>;
  getTile(name: string, z: number, x: number, y: number, opts: TileOptions & { format: 'json' }): Promise<FeatureCollection>;
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
