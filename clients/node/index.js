/**
 * netcluster-client -- a Node client for netcluster-server.
 *
 * Zero dependencies; uses the global `fetch` present since Node 18.
 *
 * Two things here are shaped by how the server is meant to be deployed rather
 * than by convenience:
 *
 *  1. **Writes fan out, reads pick one.** Every replica holds the complete index,
 *     so a position report has to reach all of them while a query only needs one.
 *     Pass `urls: [...]` and the client does that split for you.
 *
 *  2. **Reads can be pinned.** Replicas that consume updates in slightly different
 *     orders build slightly different trees, so cluster ids and groupings differ
 *     between them. A viewer whose polls bounce across replicas sees markers
 *     flicker. `client.forViewer(key)` pins that viewer's reads to one replica.
 */

/** Batch size for the auto-batching reporter.
 *
 * A single POST holds the server's write lock for its whole duration, so batch
 * size is the head-of-line delay every reader pays. Measured at 200,000 devices:
 *
 *   batch    write throughput    reader p99
 *     100        627k/s            0.43 ms
 *     500      1,401k/s            0.40 ms
 *    1000      1,693k/s            0.52 ms
 *    2000      1,936k/s            0.65 ms
 *    5000      2,063k/s            1.28 ms
 *   20000      2,176k/s            2.46 ms
 *
 * 1000 buys 78% of peak throughput for half a millisecond of stall. Going to
 * 20,000 buys 5% more and costs five times the stall.
 */
export const DEFAULT_MAX_BATCH = 1000;

export class NetClusterError extends Error {
  constructor(message, { status = 0, url = '', body = null, cause } = {}) {
    super(message, cause !== undefined ? { cause } : undefined);
    this.name = 'NetClusterError';
    /** HTTP status, or 0 if the request never got a response. */
    this.status = status;
    this.url = url;
    /** The server's parsed JSON error body, when there was one. */
    this.body = body;
  }
}

const trimSlash = (u) => String(u).replace(/\/+$/, '');
const enc = encodeURIComponent;

/** FNV-1a, for picking a stable replica from a viewer key. */
function hash32(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

export class NetClusterClient {
  /**
   * @param {object} [options]
   * @param {string} [options.url] a single server
   * @param {string[]} [options.urls] several replicas: writes fan out, reads pick one
   * @param {number} [options.timeoutMs=5000]
   * @param {number} [options.retries=1] retries for network errors and 5xx (never 4xx)
   * @param {(failures: {url: string, error: Error}[]) => void} [options.onReplicaError]
   *        called when a write reached some replicas but not all
   * @param {Record<string,string>} [options.headers] sent with every request
   * @param {typeof fetch} [options.fetch]
   */
  constructor(options = {}) {
    const urls =
      options.urls ?? (options.url ? [options.url] : ['http://localhost:8080']);
    if (!Array.isArray(urls) || urls.length === 0) {
      throw new TypeError('netcluster: pass options.url or a non-empty options.urls');
    }
    this.urls = urls.map(trimSlash);
    this.timeoutMs = options.timeoutMs ?? 5000;
    this.retries = options.retries ?? 1;
    this.headers = options.headers ?? {};
    this.onReplicaError = options.onReplicaError;
    this.fetch = options.fetch ?? globalThis.fetch;
    if (typeof this.fetch !== 'function') {
      throw new TypeError(
        'netcluster: no global fetch. Use Node 18+, or pass options.fetch.'
      );
    }
    this._rr = 0;
    this._pin = null;
  }

  // ------------------------------------------------------------- transport --

  _readBase() {
    if (this._pin !== null) return this._pin;
    if (this.urls.length === 1) return this.urls[0];
    return this.urls[this._rr++ % this.urls.length];
  }

  async _req(base, path, { method = 'GET', body, raw = false, signal } = {}) {
    const url = base + path;
    let last;
    for (let attempt = 0; attempt <= this.retries; attempt++) {
      let res;
      try {
        res = await this.fetch(url, {
          method,
          headers: {
            ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
            ...this.headers,
          },
          body: body !== undefined ? JSON.stringify(body) : undefined,
          signal: signal ?? AbortSignal.timeout(this.timeoutMs),
        });
      } catch (e) {
        last = new NetClusterError(`${method} ${url} failed: ${e.message}`, {
          url,
          cause: e,
        });
        continue;
      }
      if (res.ok) {
        if (res.status === 204) return null;
        return raw ? new Uint8Array(await res.arrayBuffer()) : await res.json();
      }
      let parsed = null;
      let text = '';
      try {
        text = await res.text();
        parsed = JSON.parse(text);
      } catch {
        /* not JSON; keep the raw text */
      }
      const err = new NetClusterError(
        `${method} ${path} -> ${res.status}: ${parsed?.error ?? text ?? res.statusText}`,
        { status: res.status, url, body: parsed }
      );
      // 4xx means the request is wrong. Retrying will produce the same 4xx and
      // hides the real problem behind a timeout.
      if (res.status >= 400 && res.status < 500) throw err;
      last = err;
    }
    throw last;
  }

  _read(path, opts) {
    return this._req(this._readBase(), path, opts);
  }

  /**
   * Send to every replica.
   *
   * Resolves if at least one accepted it. A replica that misses one report is
   * self-healing for anything that moves -- the next report from that device
   * corrects it -- so failing the whole ingest because one replica blinked would
   * trade a transient inconsistency for a real outage. Failures are reported
   * through `onReplicaError` so you can still alert on them.
   */
  async _write(path, opts) {
    if (this.urls.length === 1) return this._req(this.urls[0], path, opts);
    const settled = await Promise.allSettled(
      this.urls.map((b) => this._req(b, path, opts))
    );
    const failed = [];
    let first;
    settled.forEach((r, i) => {
      if (r.status === 'fulfilled') {
        if (first === undefined) first = r.value;
      } else {
        failed.push({ url: this.urls[i], error: r.reason });
      }
    });
    if (first === undefined) throw failed[0].error;
    if (failed.length && this.onReplicaError) this.onReplicaError(failed);
    return first;
  }

  // ----------------------------------------------------------- server-wide --

  health() {
    return this._read('/healthz');
  }

  listCollections() {
    return this._read('/v1/collections');
  }

  // ---------------------------------------------------------- collections --

  /**
   * Create a collection. Idempotent for the same geometry; rejects with a 409 if
   * one already exists with a different one.
   */
  createCollection(name, config = {}) {
    const body = {};
    if (config.maxZoom !== undefined) body.max_zoom = config.maxZoom;
    if (config.radius !== undefined) body.radius = config.radius;
    if (config.extent !== undefined) body.extent = config.extent;
    if (config.hysteresis !== undefined) body.hysteresis = config.hysteresis;
    if (config.categories !== undefined) body.categories = config.categories;
    if (config.ttlSeconds !== undefined) body.ttl_seconds = config.ttlSeconds;
    return this._write(`/v1/collections/${enc(name)}`, { method: 'PUT', body });
  }

  dropCollection(name) {
    return this._write(`/v1/collections/${enc(name)}`, { method: 'DELETE' });
  }

  stats(name) {
    return this._read(`/v1/collections/${enc(name)}`);
  }

  /** Full invariant check. Admin only: O(N squared). */
  verify(name) {
    return this._read(`/v1/collections/${enc(name)}/verify`);
  }

  // -------------------------------------------------------------- ingest --

  /**
   * Report positions. Upserts, so re-reporting a known device moves it.
   *
   * Chunked at `maxBatch`, because one huge request holds the server's write lock
   * for its whole duration and every reader waits behind it.
   */
  async report(name, points, { maxBatch = DEFAULT_MAX_BATCH } = {}) {
    const list = Array.isArray(points) ? points : [points];
    if (list.length === 0) return { accepted: 0 };
    let accepted = 0;
    let last = null;
    for (let i = 0; i < list.length; i += maxBatch) {
      last = await this._write(`/v1/collections/${enc(name)}/positions`, {
        method: 'POST',
        body: list.slice(i, i + maxBatch),
      });
      accepted += last?.accepted ?? 0;
    }
    return { accepted, devices: last?.devices };
  }

  remove(name, id) {
    return this._write(`/v1/collections/${enc(name)}/devices/${enc(id)}`, {
      method: 'DELETE',
    });
  }

  // --------------------------------------------------------------- query --

  /**
   * Clusters in a bounding box, as GeoJSON in the shape supercluster emits.
   * @param {[number,number,number,number]} opts.bbox [west, south, east, north]
   */
  getClusters(name, { bbox, zoom = 0, cat } = {}) {
    const q = new URLSearchParams({ zoom: String(zoom) });
    if (bbox) q.set('bbox', bbox.join(','));
    if (cat !== undefined && cat !== null && cat !== '') q.set('cat', String(cat));
    return this._read(`/v1/collections/${enc(name)}/clusters?${q}`);
  }

  /**
   * One vector tile. Returns a `Uint8Array` of MVT bytes, or the tile as GeoJSON
   * in tile-extent coordinates with `format: 'json'`.
   */
  getTile(name, z, x, y, { cat, format = 'mvt' } = {}) {
    const q = new URLSearchParams();
    if (cat !== undefined && cat !== null && cat !== '') q.set('cat', String(cat));
    const qs = q.toString();
    const ext = format === 'json' ? 'json' : 'mvt';
    return this._read(
      `/v1/collections/${enc(name)}/tiles/${z}/${x}/${y}.${ext}${qs ? '?' + qs : ''}`,
      { raw: ext === 'mvt' }
    );
  }

  /** One expansion step below a cluster, plus its `expansion_zoom`. */
  getChildren(name, clusterId) {
    return this._read(`/v1/collections/${enc(name)}/clusters/${clusterId}/children`);
  }

  /** The individual devices inside a cluster. */
  getLeaves(name, clusterId, { limit = 10, offset = 0 } = {}) {
    const q = new URLSearchParams({ limit: String(limit), offset: String(offset) });
    return this._read(
      `/v1/collections/${enc(name)}/clusters/${clusterId}/leaves?${q}`
    );
  }

  /** Which marker is this device drawn inside, at this zoom? */
  deviceCluster(name, id, zoom = 0) {
    return this._read(
      `/v1/collections/${enc(name)}/devices/${enc(id)}/cluster?zoom=${zoom}`
    );
  }

  // ------------------------------------------------------------ ergonomics --

  /**
   * A view whose reads always go to the same replica.
   *
   * Use it per map viewer, keyed on a session or user id. Replicas that consumed
   * updates in different interleavings hold slightly different trees, so a viewer
   * polling across replicas sees markers jump between groupings. Pinning costs
   * nothing and removes it.
   */
  forViewer(key) {
    const c = Object.create(Object.getPrototypeOf(this));
    Object.assign(c, this);
    c._pin = this.urls[hash32(String(key)) % this.urls.length];
    return c;
  }

  /** Bind a collection name so you stop repeating it. */
  collection(name) {
    const bind =
      (fn) =>
      (...args) =>
        fn.call(this, name, ...args);
    return {
      name,
      create: bind(this.createCollection),
      drop: bind(this.dropCollection),
      stats: bind(this.stats),
      verify: bind(this.verify),
      report: bind(this.report),
      remove: bind(this.remove),
      getClusters: bind(this.getClusters),
      getTile: bind(this.getTile),
      getChildren: bind(this.getChildren),
      getLeaves: bind(this.getLeaves),
      deviceCluster: bind(this.deviceCluster),
      reporter: (opts) => this.reporter(name, opts),
    };
  }

  /** An auto-batching reporter. See {@link Reporter}. */
  reporter(name, opts = {}) {
    return new Reporter(this, name, opts);
  }
}

/**
 * Accumulates position reports and flushes them on a timer.
 *
 * Reports coalesce by device id: a vehicle that reports ten times between two
 * flushes sends one request carrying its latest position. That is usually a large
 * reduction on its own -- devices report far more often than a map needs to
 * change -- and it is why this exists rather than a plain `setInterval` around
 * `report()`.
 */
export class Reporter {
  constructor(client, collection, options = {}) {
    this.client = client;
    this.collection = collection;
    this.flushMs = options.flushMs ?? 500;
    this.maxBatch = options.maxBatch ?? DEFAULT_MAX_BATCH;
    this.onError = options.onError;
    /** @type {Map<string, object>} */
    this.pending = new Map();
    this.stats = { queued: 0, coalesced: 0, sent: 0, requests: 0, errors: 0 };
    this._closed = false;
    this._inflight = null;
    this._timer = setInterval(() => {
      this.flush().catch(() => {});
    }, this.flushMs);
    // Never hold a process open just to run the flush timer.
    this._timer.unref?.();
  }

  /** Queue one report. Replaces any earlier unflushed report for the same id. */
  report(point) {
    if (this._closed) throw new Error('netcluster: reporter is closed');
    if (!point || typeof point.id !== 'string') {
      throw new TypeError('netcluster: a report needs a string id');
    }
    if (this.pending.has(point.id)) this.stats.coalesced++;
    this.pending.set(point.id, point);
    this.stats.queued++;
  }

  reportMany(points) {
    for (const p of points) this.report(p);
  }

  /** Send everything queued. Safe to call concurrently; overlapping calls chain. */
  async flush() {
    if (this._inflight) {
      await this._inflight;
    }
    if (this.pending.size === 0) return { accepted: 0 };
    const batch = [...this.pending.values()];
    this.pending.clear();
    this._inflight = (async () => {
      try {
        const r = await this.client.report(this.collection, batch, {
          maxBatch: this.maxBatch,
        });
        this.stats.sent += r.accepted;
        this.stats.requests += Math.ceil(batch.length / this.maxBatch);
        return r;
      } catch (e) {
        this.stats.errors++;
        // Put them back, unless a newer report has already superseded them --
        // dropping a position silently would leave a vehicle frozen on the map.
        for (const p of batch) if (!this.pending.has(p.id)) this.pending.set(p.id, p);
        if (this.onError) this.onError(e);
        else throw e;
        return { accepted: 0 };
      } finally {
        this._inflight = null;
      }
    })();
    return this._inflight;
  }

  /** Stop the timer and flush what is left. */
  async close() {
    this._closed = true;
    clearInterval(this._timer);
    await this.flush().catch((e) => {
      if (this.onError) this.onError(e);
      else throw e;
    });
  }
}

export default NetClusterClient;
