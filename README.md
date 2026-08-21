# NetCluster Server

A **clustering server**. Ingest position reports for things that move; serve
clustered vector tiles.

Geospatial databases index points for proximity — *what is near here*, *what is
inside this polygon* — and leave clustering to the client. So a map of moving
things ends up running two systems: one for the queries, and a separate
supercluster instance rebuilt on a timer for the markers. This closes that seam:
the primary index is a net hierarchy, so clustering is a first-class query and the
index is never rebuilt.

```
POST /v1/collections/fleet/positions     ->  917,000 reports/s
GET  /v1/collections/fleet/tiles/10/379/580.mvt  ->  0.18 ms, 115 markers
```

Two branches:

| branch | what |
|---|---|
| **`master`** | the server, the Node client, deployment manifests |
| **`netcluster`** | the index on its own — a direct Rust port of [netcluster-js], usable as a plain crate |

```
crates/netcluster/          the index          (no dependencies)
crates/netcluster-server/   the HTTP server    (axum, tokio)
clients/node/               the Node client    (no dependencies)
deploy/k8s/                 Kubernetes manifests
docs/DEPLOY.md              Docker, Kubernetes, GCP, AWS
```

`netcluster` is the base; `master` merges it and builds on top. Both share
`crates/netcluster`, so library work lands on `netcluster` and merges forward
cleanly.

## Measured

500,000 points, Apple M-series, one process. The JavaScript column is
[netcluster-js] measured on the same machine; memory is peak RSS of the whole
process, runtime included.

| | supercluster | netcluster-js | **this (Rust)** |
|---|---|---|---|
| insert one point | 872,000 µs *(full reload)* | 2.10 µs | **0.86 µs** |
| move one point | 872,000 µs *(full reload)* | 2.13 µs | **0.65 µs** |
| remove one point | 872,000 µs *(full reload)* | 7.94 µs | **1.76 µs** |
| peak RSS | 551 MB | 244 MB | **172 MB** |

Through HTTP, with JSON parsing, at 100,000 devices:

```
ingest (cold)     100,000 devices  0.11 s   917,000 reports/s
ingest (moves)    100,000 moves    0.11 s   952,000 reports/s
tile z=8   (35 markers)    0.12 ms    1.2 KB
tile z=10 (115 markers)    0.18 ms    3.0 KB
```

And the number that matters most — **8 clients reading while a writer saturates
the index**:

```
32,091 tile reads in 3 s   p50 0.19 ms   p99 4.58 ms
  ... while 2,780,000 positions were written concurrently
```

That is the whole reason this is a Rust service rather than Lua inside Redis. In
the Redis version a wide query blocked every other client for its full duration
(18.6 ms p50 on the primary). Here queries take `&self` and mutations take
`&mut self`, so an `RwLock` gives real concurrent readers.

## Run

```bash
docker run -p 8080:8080 renatex314/netcluster-server
```

or from source:

```bash
docker compose up                   # or: cargo run --release -p netcluster-server
```

Then open <http://localhost:8080/> for a live demo: a simulated fleet of up to
200,000 vehicles moving continuously, rendered by MapLibre straight from the
`.mvt` endpoint.

The image is 9.6 MB to pull (37 MB on disk) — a distroless base with no shell and
no package manager, and a 1.3 MB binary. It is published for **linux/amd64 and
linux/arm64**, runs as non-root under a read-only root filesystem, and its health
check is a flag on the binary (`--health`), which is why it needs no curl.

[![docker](https://img.shields.io/docker/v/renatex314/netcluster-server?label=docker&sort=semver)](https://hub.docker.com/r/renatex314/netcluster-server)
[![image size](https://img.shields.io/docker/image-size/renatex314/netcluster-server/latest)](https://hub.docker.com/r/renatex314/netcluster-server)
[![npm](https://img.shields.io/npm/v/netcluster-client?label=netcluster-client)](https://www.npmjs.com/package/netcluster-client)

```
NETCLUSTER_ADDR             0.0.0.0:8080   listen address
NETCLUSTER_SWEEP_SECONDS    10             how often to drop expired devices
NETCLUSTER_AUTO_CREATE      1              create a collection on first write
NETCLUSTER_DATA_DIR         (unset)        snapshot directory; unset = no persistence
NETCLUSTER_SNAPSHOT_SECONDS 60             snapshot interval
```

Persistence is **opt-in**. Without `NETCLUSTER_DATA_DIR` the server keeps nothing,
which is the right default when devices report on a timer — the index refills
itself in about a second per million devices. Set it when reporting is
event-driven, where a parked vehicle would otherwise never reappear after a
restart. See [docs/DEPLOY.md](docs/DEPLOY.md#persistence-when-you-want-it).

Turn `NETCLUSTER_AUTO_CREATE` **off** in production: with it on, a typo in a
collection name silently creates an empty collection instead of returning 404, and
you debug an empty map instead of reading an error.

## Node client

```bash
npm install netcluster-client
```

```js
import { NetClusterClient } from 'netcluster-client';

const fleet = new NetClusterClient({ url: 'http://localhost:8080' }).collection('fleet');
await fleet.create({ ttlSeconds: 300, categories: ['idle', 'enroute', 'delivering'] });

// batches on a timer AND coalesces by device id, so a vehicle reporting ten
// times between flushes sends one entry with its latest position
const reporter = fleet.reporter({ flushMs: 500 });
onGpsFix((f) => reporter.report({ id: f.deviceId, lng: f.lng, lat: f.lat }));

const { features } = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 12 });
const tile = await fleet.getTile(12, 1517, 2323);   // Uint8Array of MVT
```

Zero dependencies, TypeScript declarations bundled, and it knows the replication
rules below — writes fan out to every replica, reads go to one, and
`client.forViewer(sessionId)` pins a viewer so markers do not flicker.

```bash
cd clients/node
npm run example         # a guided tour of every function, then exits
npm run example:fleet   # 20,000 vehicles reporting continuously
```

`example.mjs` ends by asserting it called **every** public method on the client,
and `npm test` runs it — so a method added and not demonstrated fails the build.
Full documentation in [`clients/node/`](clients/node/).

## API

```bash
# a collection, with named categories so filters read as words
curl -X PUT localhost:8080/v1/collections/fleet -H 'content-type: application/json' \
  -d '{"max_zoom":16,"ttl_seconds":300,"categories":["idle","enroute","delivering"]}'

# report positions (batch)
curl -X POST localhost:8080/v1/collections/fleet/positions -H 'content-type: application/json' \
  -d '[{"id":"truck-1","lng":-46.6333,"lat":-23.5505,"cat":"delivering"},
       {"id":"truck-2","lng":-46.6340,"lat":-23.5510,"cat":"delivering"}]'

# vector tiles -- MapLibre and Leaflet consume these natively
curl localhost:8080/v1/collections/fleet/tiles/10/379/580.mvt

# GeoJSON, in the exact shape supercluster emits
curl 'localhost:8080/v1/collections/fleet/clusters?bbox=-47,-24,-46,-23&zoom=12'

# only the delivering ones -- precomputed, not scanned
curl 'localhost:8080/v1/collections/fleet/clusters?bbox=-47,-24,-46,-23&zoom=12&cat=delivering'

# which marker is my vehicle inside right now?
curl 'localhost:8080/v1/collections/fleet/devices/truck-1/cluster?zoom=12'
```

| | |
|---|---|
| `PUT /v1/collections/{name}` | create; idempotent, 409 on a different geometry |
| `GET /v1/collections` | list, with stats |
| `DELETE /v1/collections/{name}` | drop |
| `POST /v1/collections/{name}/positions` | batch ingest |
| `DELETE /v1/collections/{name}/devices/{id}` | remove one device |
| `GET .../devices/{id}` | is it registered? 200 with position, category and staleness, or 404 (`HEAD` for a bare check) |
| `GET .../clusters?bbox=&zoom=&cat=` | GeoJSON |
| `GET .../tiles/{z}/{x}/{y}.mvt` | vector tile (`.json` for tile-space GeoJSON) |
| `GET .../devices/{id}/cluster?zoom=` | which marker contains this device |
| `GET .../clusters/{id}/children` | one expansion step, plus `expansion_zoom` |
| `GET .../clusters/{id}/leaves?limit=&offset=` | the individual devices inside |
| `POST .../snapshot` | write a snapshot now (persistence must be on) |
| `GET .../verify` | full invariant check — admin only, `O(N²)` |
| `GET /healthz`, `GET /metrics` | liveness, Prometheus |

## Tuning the clustering

Set per collection, at creation:

```bash
curl -X PUT localhost:8080/v1/collections/fleet -H 'content-type: application/json' \
  -d '{"radius":40,"extent":512,"max_zoom":16,"hysteresis":0.25,
       "categories":["idle","enroute","delivering"],"ttl_seconds":300}'
```

| | default | |
|---|---|---|
| `radius` | `40` | cluster radius in screen pixels |
| `extent` | `512` | tile extent those pixels are measured against |
| `max_zoom` | `16` | finest zoom at which points still cluster; beyond it every point stands alone |
| `hysteresis` | `0.25` | how far an assignment stretches before a point is re-homed |
| `categories` | `[]` | filter labels; a label's position in the list is its index |
| `ttl_seconds` | `300` | drop a device that has not reported for this long |

`radius` and `extent` are one knob in two parts: what matters is the ratio. At the
defaults a cluster is 40px across on a 512px tile, so `radius: 80, extent: 1024`
clusters identically.

**Too many markers, too cluttered** — raise `radius`. 60–80 gives noticeably
fewer, larger clusters. This is almost always the right dial, and the only one most
people need.

**Clusters break apart too early as you zoom in** — raise `max_zoom`. It is the
zoom at which clustering stops entirely. Hard-capped at 20: beyond that the
fixed-point cell resolution runs out.

**Markers reshuffle distractingly while vehicles move** — raise `hysteresis`. This
is the one people do not know they want. At 0 a point is re-homed the instant it
strictly violates its covering constraint, so a vehicle idling on a boundary
flickers between two clusters. At 0.25 the existing assignment survives 25% past
that, trading a slightly looser worst-case radius — `2(1+h)·r_z` instead of
`2·r_z` — for far fewer visible changes. Try 0.5 if churn is still visible; it also
costs less CPU, because fewer moves take the repair path.

**Filtering** costs nothing extra at query time and nothing extra per update: a
point belongs to exactly one category, so it touches exactly one aggregate slice
per level regardless of how many categories exist.

**Geometry is fixed once a collection exists.** Re-`PUT`ting the same values is
idempotent; different values return **409**. That is deliberate — silently keeping
the old geometry would leave two deployments disagreeing about what a cluster
means while both believe they configured it. To change it, drop and recreate, or
use a new name. `ttl_seconds` is not geometry and can be changed the same way, but
it too requires a recreate today.

## Architecture

**This is not a database.** It holds no truth — the authority for where your
devices are lives wherever the reports come from (Kafka, MQTT, your existing
Redis, Postgres), and this is a *materialised view* of that stream. That single
fact deletes the entire durability chapter: no write-ahead log, no snapshot
format, no compaction, no replication protocol, no failover, no split-brain.

Cold start does the work persistence would have. At ~1 µs per insert, a 500,000
device fleet is rebuilt in about a second, so a process that dies is a process you
restart.

It also means the scaling model is **replication, not sharding**:

```
position stream ──┬──> replica A (full index) ──> queries
                  ├──> replica B (full index) ──> queries
                  └──> replica C (full index) ──> queries
```

Read capacity scales by adding processes. No leader, no consensus, no rebalancing,
because there is nothing to protect.

Deployment specifics — Docker, Kubernetes manifests, GKE, Cloud Run, ECS, EKS,
sizing and operational limits — are in **[docs/DEPLOY.md](docs/DEPLOY.md)**.

Two consequences worth knowing before you deploy it:

- **Do not shard geographically.** An ordinary spatial index can be split by
  region, because an R-tree or grid query is spatially local. This hierarchy is
  *globally coupled at coarse zooms* — a cluster at `z=0` spans continents, so a
  vehicle in Brazil and one in Angola can share a parent. Shard by collection
  (fleet A, fleet B), never by region.
- **Route a client stickily.** Replicas consuming updates in different interleavings
  build slightly different trees, so cluster ids and groupings can differ between
  them. Visually that is markers flickering as a client's polls bounce across
  replicas. Hashing the client or the viewport to a replica fixes it, and is far
  cheaper than forcing deterministic global ordering.

**Set a TTL.** A vehicle that stops reporting does not stop existing in the index,
and clusters quietly fill with ghosts until every count on the map reads high. The
sweep runs off the async runtime and drops devices in small batches, so it never
holds the write lock long enough to stall queries.

## How the index works

For every zoom level `z` it maintains a *net* of the live point set at scale
`r_z = radius / (extent · 2^z)`, with three invariants repaired locally on every
update:

- **Nesting** — `C_0 ⊆ C_1 ⊆ … ⊆ C_maxZoom ⊆ P`
- **Separation** — distinct centers of `C_z` are more than `r_z` apart
- **Covering** — every `p ∈ C_{z+1} \ C_z` has a parent within `r_z`

Those are the invariants of a compressed net-tree over the Web-Mercator plane. Two
guarantees follow and hold permanently: cluster radius `≤ 2·r_z`, and cluster count
`≤ |OPT(r_z/2)|`. Full detail in [`crates/netcluster/README.md`](crates/netcluster/README.md).

## Tests

```bash
cargo test --release
```

- The index re-derives every invariant from scratch and is checked against long
  randomised operation streams, plus a **differential test that replays 15,680
  operations recorded from the JavaScript implementation** and compares the
  complete device-to-representative map at every zoom.
- The MVT encoder is checked by decoding its own wire format.
- The server layer is checked for id interning, category resolution, expiry, tile
  coverage, and **concurrent readers running against a live writer**.
- The Node client has 17 integration tests that spawn the real server binary and
  drive it over HTTP, so they exercise the wire format rather than a mock
  (`cd clients/node && npm test`).

## What this deliberately does not do

Geofencing, polygon `WITHIN`/`INTERSECTS`, webhooks, persistence, replication.
Those belong to a full geospatial database — PostGIS and its peers — and you can
run this alongside one for the map, which is the part they leave to you.

## License

MIT

[netcluster-js]: https://github.com/renatex314/NetCluster
