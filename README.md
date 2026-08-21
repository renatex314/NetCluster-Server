# NetCluster Server

A **clustering server**. Ingest position reports for things that move; serve
clustered vector tiles.

Tile38 answers *"which devices are near here"* and *"which just crossed this
fence"*. It has no clustering — so people run Tile38 **and** a separate
supercluster instance that gets rebuilt on a timer for the map. This closes that
seam: the primary index is a net hierarchy, so clustering is a first-class query
and the index is never rebuilt.

```
POST /v1/collections/fleet/positions     ->  917,000 reports/s
GET  /v1/collections/fleet/tiles/10/379/580.mvt  ->  0.18 ms, 115 markers
```

Two branches:

| branch | what |
|---|---|
| **`master`** | the server |
| **`netcluster`** | the index on its own — a direct Rust port of [netcluster-js], usable as a plain crate |

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
cargo run --release -p netcluster-server
```

Then open <http://localhost:8080/> for a live demo: a simulated fleet of up to
200,000 vehicles moving continuously, rendered by MapLibre straight from the
`.mvt` endpoint.

```
NETCLUSTER_ADDR             0.0.0.0:8080   listen address
NETCLUSTER_SWEEP_SECONDS    10             how often to drop expired devices
NETCLUSTER_AUTO_CREATE      1              create a collection on first write
```

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
| `GET .../clusters?bbox=&zoom=&cat=` | GeoJSON |
| `GET .../tiles/{z}/{x}/{y}.mvt` | vector tile (`.json` for tile-space GeoJSON) |
| `GET .../devices/{id}/cluster?zoom=` | which marker contains this device |
| `GET .../clusters/{id}/children` | one expansion step, plus `expansion_zoom` |
| `GET .../clusters/{id}/leaves?limit=&offset=` | the individual devices inside |
| `GET .../verify` | full invariant check — admin only, `O(N²)` |
| `GET /healthz`, `GET /metrics` | liveness, Prometheus |

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

Two consequences worth knowing before you deploy it:

- **Do not shard geographically.** Tile38 can, because an R-tree query is spatially
  local. This hierarchy is *globally coupled at coarse zooms* — a cluster at `z=0`
  spans continents, so a vehicle in Brazil and one in Angola can share a parent.
  Shard by collection (fleet A, fleet B), never by region.
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

## What this deliberately does not do

Geofencing, polygon `WITHIN`/`INTERSECTS`, webhooks, persistence, replication. If
you need those, you need Tile38 or PostGIS — and you can run this alongside them
for the map, which is the thing they do not do.

## License

MIT

[netcluster-js]: https://github.com/renatex314/NetCluster
