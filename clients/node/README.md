# netcluster-client

Node client for [netcluster-server]: report moving positions, read clustered
vector tiles.

Zero dependencies. Node 18+.

```bash
npm install netcluster-client
```

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

// GeoJSON, in the shape supercluster emits
const { features } = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 12 });

// only the delivering ones -- precomputed, not scanned
const busy = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 12, cat: 'delivering' });

// raw MVT bytes, for serving straight to MapLibre or Leaflet
const tile = await fleet.getTile(12, 1517, 2323);   // Uint8Array
```

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
| `createCollection(name, config)` | idempotent; rejects 409 on a different geometry |
| `dropCollection(name)` | |
| `listCollections()` / `stats(name)` | |
| `report(name, points, { maxBatch })` | upserts; chunked |
| `remove(name, id)` | |
| `has(name, id)` | is this device registered? |
| `getDevice(name, id)` | position, category and staleness, or `null` |
| `getClusters(name, { bbox, zoom, cat })` | GeoJSON `FeatureCollection` |
| `getTile(name, z, x, y, { cat, format })` | `Uint8Array` of MVT, or `format: 'json'` |
| `getChildren(name, clusterId)` | one expansion step, plus `expansion_zoom` |
| `getLeaves(name, clusterId, { limit, offset })` | the individual devices |
| `deviceCluster(name, id, zoom)` | which marker contains this device |
| `snapshot(name)` | write a snapshot now; rejects `persistence_disabled` if off |
| `verify(name)` | full invariant check — admin only, `O(N²)` |
| `health()` | |
| `reporter(name, opts)` | the batching reporter above |
| `collection(name)` | bind the name into every call |
| `forViewer(key)` | pin reads to one replica |

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
[docs/DEPLOY.md]: https://github.com/renatex314/NetCluster-Server/blob/master/docs/DEPLOY.md
