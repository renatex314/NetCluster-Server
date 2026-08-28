# netcluster-client

Node client **and CLI** for [netcluster-server]: report moving positions, read
clustered vector tiles.

Zero dependencies. Node 18+.

```bash
npm install netcluster-client      # library + the `netcluster` command
npx netcluster-client health       # or use the CLI without installing
```

## CLI

```bash
netcluster create fleet --categories idle,enroute,delivering --ttl 300
netcluster seed fleet --count 50000          # a simulated fleet, for demos and load tests
netcluster load fleet points.geojson         # bulk-load GeoJSON
netcluster clusters fleet --zoom 6           # what the map would draw
netcluster clusters fleet --zoom 6 --filter client=7 --filter status=enroute
netcluster where fleet v42 --zoom 10         # which marker holds this device
netcluster watch                             # live devices, ingest rate, memory, snapshot age
```

```
SERVER       health, collections, watch
COLLECTIONS  create, drop, stats, verify, snapshot
DEVICES      report, import, load, seed, get, has, rm
QUERIES      clusters, where, children, leaves, tile
```

Points at `http://localhost:8080` unless you set `--url` or `NETCLUSTER_URL`.

Every command takes `--json`, so it composes:

```bash
netcluster stats fleet --json | jq .devices
netcluster has fleet v42 && echo "still reporting"      # exit 0 present, 1 absent
cat positions.ndjson | netcluster import fleet -
cat fleet.geojson    | netcluster load fleet -            # NDJSON features work too
```

Exit codes are meant for scripts: **0** fine, **1** the request failed or the
answer was no, **2** you typed it wrong. `drop` refuses a populated collection
unless you pass `--yes`.

`netcluster help`, or `netcluster <command> --help`.

```js
import { NetClusterClient } from 'netcluster-client';

const nc = new NetClusterClient({ url: 'http://localhost:8080' });
const fleet = nc.collection('fleet');

await fleet.create({
  maxZoom: 16,
  ttlSeconds: 300,
  categories: ['idle', 'enroute', 'delivering'],
});

await fleet.report([
  { id: 'truck-1', lng: -46.6333, lat: -23.5505, cat: 'delivering' },
  { id: 'truck-2', lng: -46.6340, lat: -23.5510, cat: 'delivering' },
]);

// already GeoJSON? send it as-is -- same endpoint, same upsert
await fleet.reportGeoJSON(await (await fetch('/fleet.geojson')).json());

// GeoJSON, in the shape supercluster emits
const { features } = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 12 });

// only the delivering ones -- precomputed, not scanned
const busy = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 12, cat: 'delivering' });

// filters combine, and a device may hold several values -- see "Filtering"
const mine = await fleet.getClusters({
  bbox: [-47, -24, -46, -23], zoom: 12, filter: { client: 7, status: 'enroute' },
});

// raw MVT bytes, for serving straight to MapLibre or Leaflet
const tile = await fleet.getTile(12, 1517, 2323);   // Uint8Array
```

## Attaching data to a device

```js
await fleet.report([{
  id: 'truck-1', lng: -46.6333, lat: -23.5505,
  props: { plate: 'ABC-1234', driver: 'Ana', battery: 87 },
}]);

(await fleet.getDevice('truck-1')).props;   // { plate: 'ABC-1234', ... }
```

| `props` | effect |
|---|---|
| omitted | unchanged — the ordinary position update |
| `{...}` | replaces the whole object |
| `{}` | clears |

Omit `props` and the device keeps what it had — positions arrive far more often
than attributes change, so a position report should not have to resend the plate to
avoid erasing it. The reporter carries properties forward across coalescing for the
same reason.

**There is no partial update.** `props` replaces wholesale, so changing one field
means resending the object. Merge semantics on nested values are ambiguous — given
a stored `{nested: {a: 1}}`, a patch of `{nested: {b: 2}}` could reasonably replace
or merge — and replacing is not. Keep `props` small and it does not come up.

Single points return their props as the GeoJSON `properties`; the device id is on
the feature itself (`feature.id`). Clusters carry none. In vector tiles, top-level
scalars become MVT tags so you can style by them.

Capped by `maxPropsBytes` (default 1024). `props` is payload, never indexed —
anything you filter or group by belongs in a [dimension](#filtering) instead.

## Filtering

A map usually needs more than one filter at a time: *which client owns this
vehicle* **and** *what is it doing*. A vehicle can also belong to several clients
at once. Declare the properties you filter on, and the combinations a query may
name.

```js
await fleet.create({
  dimensions: [
    // `multi`: one vehicle can be operated for several clients
    { name: 'client', values: ['1', '7', '22'], multi: true },
    { name: 'status', values: ['idle', 'enroute'] },
  ],
  // the combinations a query may name -- see "What it costs" below
  filters: [['client'], ['status'], ['client', 'status']],
});
```

### Reporting values

Values ride alongside the position, in `dims`:

```js
await fleet.report([
  { id: 'truck-1', lng: -46.6333, lat: -23.5505,
    dims: { client: ['1', '7'], status: 'enroute' } },
]);
```

or, in GeoJSON, under each dimension's own name in `properties`:

```js
await fleet.reportGeoJSON({
  type: 'FeatureCollection',
  features: [{
    type: 'Feature', id: 'truck-1',
    geometry: { type: 'Point', coordinates: [-46.6333, -23.5505] },
    properties: { client: [1, 7], status: 'enroute', plate: 'ABC-1234' },
  }],
});
```

`plate` there is ordinary payload: stored, handed back, never indexed.

| `dims` | effect |
|---|---|
| omitted | unchanged — the ordinary position update |
| `{...}` | re-files the device, even if it has not moved |

Both directions matter. A GPS ping every two seconds must not re-file a vehicle
into whatever sits at the first value; and **a status change does not move the
vehicle**, so re-reporting it where it already is *is* the whole update:

```js
await fleet.report([{ id: 'truck-1', lng, lat, dims: { status: 'idle' } }]);
```

### Querying

```js
await fleet.getClusters({ bbox, zoom, filter: { client: 7 } });
await fleet.getClusters({ bbox, zoom, filter: { client: 7, status: 'enroute' } });
await fleet.getClusters({ bbox, zoom });                    // everything
```

Tiles take the same `filter`. A query names **one value per dimension** — a device
may hold several, but "client 7" is a question with an answer and "client 7 or 9"
is two.

The combination must match a declared shape exactly. Anything else is a 400 naming
what is declared, never an empty result: a filter that silently matched nothing
looks exactly like a fleet that has gone quiet.

```
unknown filter "plate"; this collection has client, status
no declared filter combines [client, status]; this collection allows [client], [status]
```

### From the CLI

```bash
netcluster create fleet --dimension 'client=1,7,22:multi'                         --dimension 'status=idle,enroute'                         --shape client --shape status --shape client,status

netcluster report fleet truck-1 -46.6333 -23.5505 --dim client=1,7 --dim status=enroute
netcluster clusters fleet --zoom 12 --filter client=7 --filter status=enroute
```

`--dimension`, `--shape`, `--dim` and `--filter` may each be repeated.

### What it costs

Each declared shape carries its own running total, because a conjunction cannot be
assembled from its parts — knowing how many vehicles are `client 7`, and how many
are `enroute`, says nothing about how many are both. So `[['client'], ['status'],
['client','status']]` costs about three times what `[['client']]` does. Declare the
combinations your UI offers and no more; leaving `filters` out gives each dimension
on its own, which is the cheapest useful setting.

Reads stay fast either way — a filtered query is typically *faster* than an
unfiltered one, since a subtree holding none of the requested value is skipped
whole. Sizing and the measured numbers are in the JavaScript library's
[`docs/FILTERING.md`][filtering], which documents the same mechanism.

### What it cannot do

Substring search, ranges, `OR` across values, and anything read out of `props`.
A plate box is a registry lookup rather than a map query — keep the text in your
own database, resolve it to ids there, and ask the index only about those.

And **do not reach for the whole fleet and filter it yourself.** `getClusters`
clusters at every zoom, so it is not a device listing: zoom is clamped to
`maxZoom`, and vehicles parked closer than the radius at that zoom (~44 m at the
defaults) come back as a single cluster with no id and no props, which a filter of
your own silently skips — a depot disappears. Use `getLeaves(clusterId)` to reach
the members.

## Tuning the clustering

```js
await fleet.create({
  radius: 40,        // cluster radius in screen pixels -- the main dial
  extent: 512,       // tile extent those pixels are measured against
  maxZoom: 16,       // finest zoom at which points still cluster (max 20)
  hysteresis: 0.25,  // how far an assignment stretches before a point is re-homed
  categories: ['idle', 'enroute', 'delivering'],
  ttlSeconds: 300,
});
```

Only the **ratio** of `radius` to `extent` matters, so `radius: 80, extent: 1024`
clusters identically to the defaults.

Raise `radius` for fewer, larger clusters — the dial most people need. Raise
`maxZoom` if clusters break apart too early as you zoom in. Raise `hysteresis` if
markers reshuffle distractingly while vehicles move: at 0 a vehicle idling on a
cluster boundary flickers between two, and 0.25 lets the existing assignment
survive 25% past the strict constraint.

Geometry is fixed once a collection exists — `create()` is idempotent for the same
values and rejects **409** for different ones. Full detail in the
[server README](https://github.com/renatex314/NetCluster-Server#tuning-the-clustering).

## Reporting a live fleet

Do not call `report()` per device per tick. Use the reporter: it batches on a
timer and **coalesces by device id**, so a vehicle that reports ten times between
two flushes sends one entry carrying its latest position. Devices report far more
often than a map needs to change, so this is usually a large reduction on its own.

```js
const reporter = fleet.reporter({ flushMs: 500 });

onGpsFix((fix) => reporter.report({ id: fix.deviceId, lng: fix.lng, lat: fix.lat }));

// on shutdown
await reporter.close();
```

`reporter.stats` tells you what it saved: `{ queued, coalesced, sent, requests, errors }`.

A failed flush **requeues** the positions rather than dropping them — unless a
newer report for that device has already arrived, in which case the newer one
wins. Silently dropping a position leaves a vehicle frozen on the map.

### Batch size

One request holds the server's write lock for its whole duration, so batch size is
the head-of-line delay every reader pays. Measured at 200,000 devices with four
concurrent readers:

| batch | write throughput | reader p99 |
|---|---|---|
| 100 | 627k reports/s | 0.43 ms |
| 500 | 1,401k | 0.40 ms |
| **1000** *(default)* | **1,693k** | **0.52 ms** |
| 2000 | 1,936k | 0.65 ms |
| 5000 | 2,063k | 1.28 ms |
| 20000 | 2,176k | 2.46 ms |

1000 buys 78% of peak throughput for half a millisecond of stall. Raise it only if
ingest is genuinely your bottleneck.

## Several replicas

Every netcluster replica holds the **complete** index, so a position report has to
reach all of them while a query only needs one. Pass `urls` and the client does
that split:

```js
const nc = new NetClusterClient({
  urls: ['http://pod-a:8080', 'http://pod-b:8080', 'http://pod-c:8080'],
  onReplicaError: (failures) => log.warn('replica missed a write', failures),
});
```

Writes fan out to all; reads go to one. A write resolves as long as *one* replica
accepted it — a replica that misses a report self-heals when the device reports
again a second later, so failing the whole ingest because a pod was rolling would
trade a transient inconsistency for a real outage.

**Pin each viewer.** Replicas that consume updates in slightly different orders
build slightly different trees, so cluster ids and groupings differ between them. A
viewer polling across replicas sees markers jump. One line fixes it:

```js
const view = nc.forViewer(session.id);   // this viewer's reads always hit one replica
const fc = await view.getClusters('fleet', { bbox, zoom });
```

Discovering replicas from a Kubernetes headless Service, and the rest of the
topology, is in [docs/DEPLOY.md].

## API

Every method exists both on the client (`nc.getClusters('fleet', …)`) and on a
bound collection (`nc.collection('fleet').getClusters(…)`).

| | |
|---|---|
| `createCollection(name, config)` | idempotent; rejects 409 on a different geometry. `dimensions` / `filters` declare what you can [filter](#filtering) on |
| `dropCollection(name)` | |
| `listCollections()` / `stats(name)` | |
| `report(name, points, { maxBatch })` | upserts; chunked. A point may carry `dims` and `props` |
| `reportGeoJSON(name, geojson, { maxBatch, idProperty, catProperty })` | the same, with GeoJSON on the wire |
| `remove(name, id)` | |
| `has(name, id)` | is this device registered? |
| `getDevice(name, id)` | position, category and staleness, or `null` |
| `getClusters(name, { bbox, zoom, cat, filter })` | GeoJSON `FeatureCollection` |
| `getTile(name, z, x, y, { cat, filter, format })` | `Uint8Array` of MVT, or `format: 'json'` |
| `getChildren(name, clusterId)` | one expansion step, plus `expansion_zoom` |
| `getLeaves(name, clusterId, { limit, offset })` | the individual devices |
| `deviceCluster(name, id, zoom)` | which marker contains this device |
| `snapshot(name)` | write a snapshot now; rejects `persistence_disabled` if off |
| `verify(name)` | full invariant check — admin only, `O(N²)` |
| `health()` | |
| `reporter(name, opts)` | the batching reporter above |
| `collection(name)` | bind the name into every call |
| `forViewer(key)` | pin reads to one replica |

### GeoJSON

```js
await fleet.reportGeoJSON({ type: 'FeatureCollection', features: [...] });
await fleet.reportGeoJSON([feature1, feature2]);   // a bare array works too
await fleet.reportGeoJSON(feature);                // or one on its own
```

Same endpoint and same upsert semantics as `report` — only the wire format
differs. Chunked at `maxBatch` for the same reason: one huge request holds the
server's write lock for its whole duration.

The id comes from `feature.id`, where GeoJSON says it goes, then `properties.id`.
`properties` is stored verbatim, `null` leaves what is stored alone, and
`properties.cat` (or `category`) sets the category. A third coordinate is
altitude and is ignored; anything that is not a Point geometry is rejected rather
than quietly reduced to a centroid.

```js
// where the file keeps its id somewhere else
await fleet.reportGeoJSON(features, { idProperty: 'plate', catProperty: 'status' });
```

`idProperty` is strict: a feature missing that property is rejected rather than
falling back to `feature.id`, because a silent fallback keys half a fleet one way
and half the other.

Rejections arrive as `NetClusterError` with `e.body.code === 'bad_geojson'` and a
message naming the feature by index:

```
features[8123] has a null geometry, so it has no position to cluster
```

Ingest runs at roughly 830,000 features/s against 940,000 reports/s for the
compact form — GeoJSON is about twice the bytes per point. Full table in the
[server README](../../README.md#geojson).

### Registration

```js
await fleet.has('truck-1');          // true
await fleet.remove('truck-1');
await fleet.has('truck-1');          // false

const d = await fleet.getDevice('truck-2');
// { id, lng, lat, cat: 'delivering', cat_index: 2, last_seen_ms, age_ms }
```

`has` asks the index, not "have we ever seen this id" — a device that was removed,
or that expired because it went quiet, answers `false`. Compare `age_ms` against
the collection's `ttl_seconds` to see how close a device is to being swept.

An unknown **collection** still throws rather than answering `false`. Two very
different situations share the 404, so the server tags them (`device_not_registered`
versus `no_such_collection`) and only the first becomes `false` — otherwise a typo
in a collection name becomes a map that is quietly empty.

### Errors

Failures throw `NetClusterError` with `status`, `url` and the server's parsed
`body`:

```js
try {
  await fleet.getClusters({ zoom: 12, cat: 'delivring' });
} catch (e) {
  e.status;         // 400
  e.body.error;     // 'unknown category "delivring"; this collection has [...]'
}
```

4xx is never retried — the request is wrong, and retrying hides the real problem
behind a timeout. Network errors and 5xx are retried `retries` times (default 1).

TypeScript declarations are bundled; there is no `@types` package to install.

## Examples

```bash
docker compose up          # from the repo root, or: cargo run --release -p netcluster-server

npm run example            # a guided tour of every function, then exits
npm run example:fleet      # 20,000 vehicles reporting continuously
```

`example.mjs` walks the whole API in order — collections, reporting, querying,
filtering, expansion, leaves, tiles, the reporter, replica pinning, errors — and
ends by asserting it called **every** public method on `NetClusterClient` and
`Reporter`. `npm test` runs it, so a method added to the client and not
demonstrated fails the build.

## Test

```bash
cargo build --release --bin netcluster-server   # from the repo root
npm test                                        # spawns the real server
```

## License

MIT

[netcluster-server]: https://github.com/renatex314/NetCluster-Server
[filtering]: https://github.com/renatex314/NetCluster/blob/main/docs/FILTERING.md
[docs/DEPLOY.md]: https://github.com/renatex314/NetCluster-Server/blob/master/docs/DEPLOY.md
