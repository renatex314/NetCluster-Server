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

// -- 3b. free-form attributes -------------------------------------------------
h('3b. props  — attach anything to a device');
await fleet.report([{
  id: 'truck-1', lng: -46.6333, lat: -23.5505, cat: 'delivering',
  props: { plate: 'ABC-1234', driver: 'Ana', battery: 87, tags: ['cold'] },
}]);
const withProps = await fleet.getDevice('truck-1');
p(`getDevice('truck-1').props → ${JSON.stringify(withProps.props)}`);

// Positions arrive far more often than attributes change, so a report without
// `props` leaves them alone rather than erasing them.
await fleet.report([{ id: 'truck-1', lng: -46.6334, lat: -23.5506 }]);
p(`after a plain position report → ${JSON.stringify((await fleet.getDevice('truck-1')).props)}`);
p(`send props: {} to clear them; nested objects and arrays survive intact`);

// -- 3c. GeoJSON in ------------------------------------------------------------
h('3c. reportGeoJSON()  — the same endpoint, GeoJSON on the wire');
// Whatever produced your points -- a .geojson file, a PostGIS query, a Mapbox
// source -- is probably already emitting this shape. It upserts exactly as
// report() does; the id comes from `id` where GeoJSON puts it, and `properties`
// is stored verbatim.
const geoRes = await fleet.reportGeoJSON({
  type: 'FeatureCollection',
  features: [
    { type: 'Feature', id: 'van-1', properties: { plate: 'GEO-001', cat: 'idle' },
      geometry: { type: 'Point', coordinates: [-46.6400, -23.5600] } },
    // properties: null is GeoJSON for "none", and leaves stored ones alone
    { type: 'Feature', id: 'van-2', properties: null,
      geometry: { type: 'Point', coordinates: [-46.6410, -23.5610, 720] } },
  ],
});
p(`reportGeoJSON() → ${geoRes.accepted} features, ${geoRes.devices} devices`);
p(`van-1 props → ${JSON.stringify((await fleet.getDevice('van-1')).props)}`);
// The third coordinate is altitude: allowed by the spec, ignored by clustering.
p(`van-2 → ${JSON.stringify((await fleet.getDevice('van-2')).lng)}, altitude dropped`);

// Where the file keeps its id somewhere other than `id`, name the property.
// Naming it is strict on purpose: a feature missing it is rejected rather than
// silently keyed by feature.id, which would split one fleet across two id spaces.
await fleet.reportGeoJSON(
  [{ type: 'Feature', properties: { plate: 'GEO-777' },
     geometry: { type: 'Point', coordinates: [-46.642, -23.562] } }],
  { idProperty: 'plate' }
);
p(`idProperty: 'plate' → registered as ${JSON.stringify(await fleet.has('GEO-777'))}`);

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

// -- 14. registration ---------------------------------------------------------
h('14. has(), getDevice() and remove()');
p(`has('truck-1')  → ${await fleet.has('truck-1')}`);
p(`has('never-seen') → ${await fleet.has('never-seen')}`);

const info = await fleet.getDevice('truck-1');
p(`getDevice('truck-1') → ${info.lng.toFixed(4)}, ${info.lat.toFixed(4)} · ` +
  `cat ${info.cat} (${info.cat_index}) · last reported ${info.age_ms} ms ago`);
p(`getDevice('never-seen') → ${await fleet.getDevice('never-seen')}`);
// age_ms against the collection's ttl_seconds tells you how close a device is to
// being swept for going quiet.

p(`remove('rio-2') → ${JSON.stringify(await fleet.remove('rio-2'))}`);
p(`remove('rio-2') again → ${JSON.stringify(await fleet.remove('rio-2'))}`);
p(`has('rio-2') after removal → ${await fleet.has('rio-2')}`);

// A missing COLLECTION is not a missing device, and must not quietly answer
// false -- that would turn a typo in a collection name into an empty map.
try {
  await nc.has('no-such-collection', 'truck-1');
} catch (e) {
  p(`has() on an unknown collection → ${e.name} ${e.status} (code: ${e.body.code})`);
}

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

// -- 16a. persistence ---------------------------------------------------------
h('16. snapshot() and verify()');
// The server is stateless unless started with NETCLUSTER_DATA_DIR. When it is, it
// snapshots on a timer and on shutdown; this forces one now, which is what you
// want before a deliberate restart.
try {
  const snap = await fleet.snapshot();
  p(`snapshot() → ${snap.bytes} bytes on disk`);
  const st2 = await fleet.stats();
  p(`last snapshot ${new Date(st2.last_snapshot_ms).toISOString()} · ` +
    `failures ${st2.snapshot_failures} · restored at boot ${st2.restored}`);
} catch (e) {
  if (e.body?.code === 'persistence_disabled') {
    p('snapshot() → persistence is off (start the server with NETCLUSTER_DATA_DIR)');
  } else {
    throw e;
  }
}

// -- 16b. verify --------------------------------------------------------------
const v = await fleet.verify();
p(`ok=${v.ok} · ${v.detail ?? v.violation}`);
// Re-derives every structural invariant from scratch. A staging tool, not a
// dashboard: it walks every pair of centers.

// -- 16c. filtering on several properties --------------------------------------
h('16c. filters that combine');
// One category answers "only the trucks". A monitoring map usually needs two at
// once -- which client owns the vehicle, and what it is doing -- and a vehicle
// can belong to several clients, which a single category cannot express.
const owners = nc.collection('example-owners');
await owners.create({
  dimensions: [
    { name: 'client', values: ['1', '7', '22'], multi: true },
    { name: 'status', values: ['idle', 'enroute'] },
  ],
  // The combinations a query may name. Each is stored separately, so declare the
  // ones the UI actually offers and no more.
  filters: [['client'], ['status'], ['client', 'status']],
  ttlSeconds: 0,
});
await owners.report([
  { id: 'truck-1', lng: -46.6333, lat: -23.5505, dims: { client: ['7', '22'], status: 'enroute' } },
  { id: 'truck-2', lng: -46.6340, lat: -23.5510, dims: { client: ['7'], status: 'idle' } },
  { id: 'truck-3', lng: -46.6350, lat: -23.5520, dims: { client: ['1'], status: 'enroute' } },
]);
const box = [-47, -24, -46, -23];
const devices = (fc) => fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
p(`everything            ${devices(await owners.getClusters({ bbox: box, zoom: 16 }))}`);
p(`client 7              ${devices(await owners.getClusters({ bbox: box, zoom: 16, filter: { client: 7 } }))}`);
p(`en route              ${devices(await owners.getClusters({ bbox: box, zoom: 16, filter: { status: 'enroute' } }))}`);
p(`client 7 AND en route ${devices(await owners.getClusters({ bbox: box, zoom: 16, filter: { client: 7, status: 'enroute' } }))}`);

// A status change does not move the vehicle: report it where it already is.
await owners.report([
  { id: 'truck-2', lng: -46.6340, lat: -23.5510, dims: { client: ['7'], status: 'enroute' } },
]);
p(`after truck-2 departs ${devices(await owners.getClusters({ bbox: box, zoom: 16, filter: { client: 7, status: 'enroute' } }))}`);

// A bare position report keeps the values it already had.
await owners.report([{ id: 'truck-2', lng: -46.6341, lat: -23.5511 }]);
p(`after it moves again  ${devices(await owners.getClusters({ bbox: box, zoom: 16, filter: { client: 7, status: 'enroute' } }))}`);
await owners.drop();

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
