// A guided tour of every function in netcluster-client.
//
//   docker compose up                 # or: cargo run --release -p netcluster-server
//   node example.mjs
//
// Runs to completion and exits. For a continuous simulated fleet, see
// example-fleet.mjs.
//
// The last section checks that this file actually called every public method, so
// the example cannot quietly fall behind the library.

import {
  NetClusterClient,
  NetClusterError,
  Reporter,
  DEFAULT_MAX_BATCH,
} from './index.js';

// ---------------------------------------------------------------------------
// Coverage instrumentation. Patch the prototypes before anything is constructed,
// so `collection()`'s bound methods pick up the wrappers too.
const covered = new Set();
function instrument(cls) {
  const names = [];
  for (const name of Object.getOwnPropertyNames(cls.prototype)) {
    if (name === 'constructor' || name.startsWith('_')) continue;
    const orig = Object.getOwnPropertyDescriptor(cls.prototype, name)?.value;
    if (typeof orig !== 'function') continue;
    names.push(`${cls.name}.${name}`);
    cls.prototype[name] = function (...args) {
      covered.add(`${cls.name}.${name}`);
      return orig.apply(this, args);
    };
  }
  return names;
}
const expected = [...instrument(NetClusterClient), ...instrument(Reporter)];
// ---------------------------------------------------------------------------

const URL = process.env.NETCLUSTER_URL ?? 'http://localhost:8080';
const h = (s) => console.log(`\n\x1b[1m${s}\x1b[0m`);
const p = (...a) => console.log('   ', ...a);

// -- 1. connect ---------------------------------------------------------------
// Every option, with its default. `url` for a single server; `urls` for several
// replicas, where writes fan out to all and reads go to one.
const nc = new NetClusterClient({
  url: URL,
  timeoutMs: 5000,      // per request
  retries: 1,           // network errors and 5xx only, never 4xx
  headers: {},          // sent with every request
  // urls: ['http://a:8080', 'http://b:8080'],
  // onReplicaError: (failures) => console.warn('replica missed a write', failures),
  // fetch: customFetch,
});

h('1. health()');
try {
  const health = await nc.health();
  p(`${health.status} · ${health.collections} collections · ${health.devices} devices · up ${health.uptime_ms} ms`);
} catch (e) {
  console.error(`\nCannot reach ${URL}. Start the server first:\n  docker compose up\n`);
  process.exit(1);
}

// -- 2. create a collection ---------------------------------------------------
h('2. createCollection() and collection()');
const NAME = 'tour';
await nc.dropCollection(NAME).catch(() => {}); // start clean; ignore "not found"
const created = await nc.createCollection(NAME, {
  maxZoom: 16,          // finest zoom at which points still cluster
  radius: 40,           // cluster radius in screen pixels
  extent: 512,          // tile extent those pixels are measured against
  hysteresis: 0.25,     // covering slack: fewer visible cluster changes under motion
  ttlSeconds: 300,      // drop a device that has not reported for this long
  categories: ['idle', 'enroute', 'delivering'],
});
p(`created=${created.created}, categories=${created.collection.categories.join(', ')}`);

// `collection()` binds the name so you stop repeating it. Everything below could
// equally be written nc.getClusters(NAME, ...).
const fleet = nc.collection(NAME);
p(`bound collection: ${fleet.name}`);

// -- 3. report positions ------------------------------------------------------
h('3. report()');
// `cat` takes a label from the collection, or its index.
const reported = await fleet.report([
  { id: 'truck-1', lng: -46.6333, lat: -23.5505, cat: 'delivering' },
  { id: 'truck-2', lng: -46.6340, lat: -23.5510, cat: 'delivering' },
  { id: 'truck-3', lng: -46.6350, lat: -23.5520, cat: 'idle' },
  { id: 'truck-4', lng: -46.7000, lat: -23.6000, cat: 1 },        // 1 === 'enroute'
  { id: 'rio-1',   lng: -43.1729, lat: -22.9068, cat: 'enroute' },
  { id: 'rio-2',   lng: -43.1740, lat: -22.9080, cat: 'delivering' },
]);
p(`accepted ${reported.accepted}, index now holds ${reported.devices}`);

// A big list is chunked, because one request holds the server's write lock for its
// whole duration and every reader waits behind it.
const bulk = Array.from({ length: 2500 }, (_, i) => ({
  id: `bulk-${i}`,
  lng: -46.63 + (Math.random() - 0.5) * 0.4,
  lat: -23.55 + (Math.random() - 0.5) * 0.4,
  cat: i % 3,
}));
const bulkRes = await fleet.report(bulk, { maxBatch: DEFAULT_MAX_BATCH });
p(`bulk: ${bulkRes.accepted} reports sent in ${Math.ceil(bulk.length / DEFAULT_MAX_BATCH)} requests of ${DEFAULT_MAX_BATCH}`);

// -- 4. what is on this server ------------------------------------------------
h('4. listCollections()');
const { collections } = await nc.listCollections();
for (const c of collections) p(`${c.name}: ${c.devices} devices, maxZoom ${c.max_zoom}, ttl ${c.ttl_seconds}s`);

// -- 5. stats -----------------------------------------------------------------
h('5. stats()');
const st = await fleet.stats();
p(`${st.devices} devices · ${(st.memory_bytes / 1e6).toFixed(1)} MB · ${st.grid_entries} grid entries`);
p(`ingested ${st.ingested} · queries ${st.queries} · expired ${st.expired} · fast-path ${st.moves_fast_pct.toFixed(1)}%`);
p(`centers per zoom: ${st.centers_per_level.slice(0, 8).join(', ')} …`);

// -- 6. the main query --------------------------------------------------------
h('6. getClusters()');
const BBOX = [-60, -35, -30, -10];
for (const zoom of [2, 6, 10, 16]) {
  const fc = await fleet.getClusters({ bbox: BBOX, zoom });
  const shown = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  p(`zoom ${String(zoom).padStart(2)}: ${String(fc.features.length).padStart(4)} markers covering ${shown} devices`);
}
// Every zoom is a partition: the markers always account for every device exactly
// once, which is the property a map actually depends on.

// -- 7. filtering -------------------------------------------------------------
h('7. getClusters({ cat })  — filtering by category');
for (const cat of ['idle', 'enroute', 'delivering']) {
  const fc = await fleet.getClusters({ bbox: BBOX, zoom: 8, cat });
  const n = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  p(`${cat.padEnd(10)} ${String(fc.features.length).padStart(3)} markers · ${n} devices`);
}
const byIndex = await fleet.getClusters({ bbox: BBOX, zoom: 8, cat: 2 });
p(`by index (2 === 'delivering'): ${byIndex.features.length} markers`);
// This costs the same as an unfiltered query. The counts are precomputed per
// category, not scanned.

// -- 8. expanding a cluster ---------------------------------------------------
h('8. getChildren()');
const z4 = await fleet.getClusters({ bbox: BBOX, zoom: 4 });
const big = z4.features
  .filter((f) => f.properties.cluster)
  .sort((a, b) => b.properties.point_count - a.properties.point_count)[0];
p(`largest cluster at zoom 4: ${big.properties.point_count_abbreviated} devices (id ${big.properties.cluster_id})`);
const kids = await fleet.getChildren(big.properties.cluster_id);
p(`splits at zoom ${kids.expansion_zoom} into ${kids.features.length}: ` +
  kids.features.map((f) => f.properties.point_count ?? 1).join(' + '));
// Click a cluster on a map, ease to expansion_zoom, and it comes apart.

// -- 9. what is inside --------------------------------------------------------
h('9. getLeaves()');
const page1 = await fleet.getLeaves(big.properties.cluster_id, { limit: 5, offset: 0 });
const page2 = await fleet.getLeaves(big.properties.cluster_id, { limit: 5, offset: 5 });
p(`first 5:  ${page1.features.map((f) => f.properties.id).join(', ')}`);
p(`next  5:  ${page2.features.map((f) => f.properties.id).join(', ')}`);

// -- 10. find one device ------------------------------------------------------
h('10. deviceCluster()  — which marker is my vehicle inside?');
for (const zoom of [3, 9, 16]) {
  const f = await fleet.deviceCluster('truck-1', zoom);
  const what = f.properties.cluster
    ? `a cluster of ${f.properties.point_count}`
    : 'drawn on its own';
  p(`zoom ${String(zoom).padStart(2)}: truck-1 is ${what} at ${f.geometry.coordinates.map((c) => c.toFixed(4)).join(', ')}`);
}

// -- 11. vector tiles ---------------------------------------------------------
h('11. getTile()');
// Web-Mercator tile coordinates for São Paulo at zoom 10.
const [z, x, y] = [10, 379, 580];
const mvt = await fleet.getTile(z, x, y);
p(`${z}/${x}/${y}.mvt → ${mvt.constructor.name}, ${mvt.length} bytes` +
  ` (protobuf field ${mvt[0] >> 3}, wire type ${mvt[0] & 7})`);
// Serve these straight to MapLibre or Leaflet; the browser runs no clustering code.

const asJson = await fleet.getTile(z, x, y, { format: 'json' });
p(`same tile as GeoJSON: ${asJson.features.length} features in tile-extent coordinates`);
const filteredTile = await fleet.getTile(z, x, y, { cat: 'delivering' });
p(`filtered to 'delivering': ${filteredTile.length} bytes`);

// -- 12. the reporter ---------------------------------------------------------
h('12. reporter()  — batching and coalescing');
const reporter = fleet.reporter({
  flushMs: 500,                    // flush interval
  maxBatch: DEFAULT_MAX_BATCH,     // points per request
  onError: (e) => console.warn('    flush failed:', e.message),
});

// A vehicle reporting many times between flushes collapses to one entry carrying
// its latest position. Devices report far more often than a map needs to change,
// so this is usually a large reduction on its own.
for (let i = 0; i < 20; i++) {
  reporter.report({ id: 'truck-1', lng: -46.6333 + i * 0.0001, lat: -23.5505 });
}
reporter.reportMany([
  { id: 'truck-2', lng: -46.6341, lat: -23.5511 },
  { id: 'truck-3', lng: -46.6351, lat: -23.5521 },
]);
p(`22 reports queued → ${reporter.pending.size} entries pending (${reporter.stats.coalesced} coalesced)`);

await reporter.flush();
p(`after flush(): sent ${reporter.stats.sent} in ${reporter.stats.requests} request(s)`);

// It also flushes on its own timer; close() stops it and drains what is left.
reporter.report({ id: 'truck-4', lng: -46.7001, lat: -23.6001 });
await reporter.close();
p(`after close(): ${JSON.stringify(reporter.stats)}`);

// -- 13. several replicas -----------------------------------------------------
h('13. forViewer()  — pinning a viewer to one replica');
// Replicas that consume updates in slightly different orders build slightly
// different trees, so a viewer whose polls bounce between them sees markers jump.
// Pinning costs nothing and removes it.
const replicated = new NetClusterClient({
  urls: [URL, 'http://replica-b:8080', 'http://replica-c:8080'],
});
for (const session of ['session-abc', 'session-xyz', 'session-abc']) {
  p(`${session} → ${replicated.forViewer(session)._readBase()}`);
}
p('same key always lands on the same replica; writes still go to all three');

// -- 14. removing a device ----------------------------------------------------
h('14. remove()');
p(`remove('rio-2') → ${JSON.stringify(await fleet.remove('rio-2'))}`);
p(`remove('rio-2') again → ${JSON.stringify(await fleet.remove('rio-2'))}`);

// -- 15. errors ---------------------------------------------------------------
h('15. NetClusterError');
let attempts = 0;
const counting = new NetClusterClient({
  url: URL,
  retries: 5,
  fetch: (...args) => { attempts++; return globalThis.fetch(...args); },
});
try {
  await counting.getClusters(NAME, { zoom: 8, cat: 'delivring' }); // typo
} catch (e) {
  p(`${e.name}: status ${e.status}`);
  p(`server said: ${e.body.error}`);
  p(`fetch calls: ${attempts} — a 4xx is never retried, because the request is`);
  p('wrong and retrying just hides it behind a timeout');
}
try {
  await new NetClusterClient({ url: 'http://127.0.0.1:1', timeoutMs: 300, retries: 0 }).health();
} catch (e) {
  p(`unreachable server → status ${e.status} (0 means no response at all)`);
}

// -- 16. verify ---------------------------------------------------------------
h('16. verify()  — admin only, O(N²)');
const v = await fleet.verify();
p(`ok=${v.ok} · ${v.detail ?? v.violation}`);
// Re-derives every structural invariant from scratch. A staging tool, not a
// dashboard: it walks every pair of centers.

// -- 17. clean up -------------------------------------------------------------
h('17. dropCollection()');
p(JSON.stringify(await fleet.drop()));

// -- coverage -----------------------------------------------------------------
const missed = expected.filter((n) => !covered.has(n));
h('coverage');
p(`${covered.size} of ${expected.length} public methods exercised`);
if (missed.length) {
  console.error(`    NOT COVERED: ${missed.join(', ')}`);
  process.exitCode = 1;
} else {
  p('every public method on NetClusterClient and Reporter was called');
}
