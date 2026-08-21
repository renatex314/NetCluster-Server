# netcluster-server

A **clustering server**. Ingest position reports for things that move; serve
clustered vector tiles.

Geospatial databases index points for proximity — *what is near here* — and leave
clustering to the client. So a map of moving things ends up running two systems:
one for the queries, and a separate [supercluster](https://github.com/mapbox/supercluster)
instance rebuilt on a timer for the markers. This closes that seam: the primary
index is a hierarchy of nets whose invariants are repaired locally on every
update, so clustering is a first-class query and **the index is never rebuilt**.

```
supercluster:  one device moved  ->  reload 500k points  ->  ~870 ms
netcluster:    one device moved  ->  ~0.65 us
```

## Run

```bash
docker run -p 8080:8080 renatex314/netcluster-server
```

Then open <http://localhost:8080/> — a live demo with up to 200,000 simulated
vehicles moving continuously, rendered by MapLibre straight from the `.mvt`
endpoint.

## Use

```bash
# a collection, with named categories so filters read as words
curl -X PUT localhost:8080/v1/collections/fleet -H 'content-type: application/json' \
  -d '{"max_zoom":16,"ttl_seconds":300,"categories":["idle","enroute","delivering"]}'

# report positions (batch)
curl -X POST localhost:8080/v1/collections/fleet/positions -H 'content-type: application/json' \
  -d '[{"id":"truck-1","lng":-46.6333,"lat":-23.5505,"cat":"delivering"}]'

# vector tiles -- MapLibre and Leaflet consume these natively
curl localhost:8080/v1/collections/fleet/tiles/10/379/580.mvt

# GeoJSON, in the exact shape supercluster emits
curl 'localhost:8080/v1/collections/fleet/clusters?bbox=-47,-24,-46,-23&zoom=12'

# only the delivering ones -- precomputed, not scanned
curl 'localhost:8080/v1/collections/fleet/clusters?bbox=-47,-24,-46,-23&zoom=12&cat=delivering'
```

There is a Node client on npm: **[`netcluster-client`](https://www.npmjs.com/package/netcluster-client)**
(zero dependencies, TypeScript declarations bundled). The index itself is also
available as a standalone JavaScript library, **[`netcluster-js`](https://www.npmjs.com/package/netcluster-js)**.

## Configuration

| variable | default | |
|---|---|---|
| `NETCLUSTER_ADDR` | `0.0.0.0:8080` | listen address |
| `NETCLUSTER_SWEEP_SECONDS` | `10` | how often to drop expired devices |
| `NETCLUSTER_AUTO_CREATE` | `1` | create a collection on first write |

Turn `NETCLUSTER_AUTO_CREATE` **off** in production: with it on, a typo in a
collection name silently creates an empty collection instead of returning 404, and
you debug an empty map instead of reading an error.

**Set a TTL.** A vehicle that stops reporting does not stop existing in the index,
and clusters quietly fill with ghosts until every count on the map reads high.

## Measured

500,000 points, one process, Apple M-series:

| | supercluster | **this** |
|---|---|---|
| insert one point | 872,000 µs *(full reload)* | **0.86 µs** |
| move one point | 872,000 µs *(full reload)* | **0.65 µs** |
| remove one point | 872,000 µs *(full reload)* | **1.76 µs** |
| peak RSS | 551 MB | **172 MB** |

Through HTTP with JSON parsing, at 100,000 devices: **917,000 reports/s** cold,
**952,000/s** for moves, tiles in 0.11–0.18 ms. Eight clients sustained 32,091
tile reads in 3 s (p50 0.19 ms, p99 4.58 ms) **while 2,780,000 positions were
written concurrently** — queries hold a read lock and mutations a write lock, so
readers genuinely run alongside the writer.

## The image

- **9.6 MB to pull**, 37 MB on disk, of which the binary is 1.3 MB
- `linux/amd64` and `linux/arm64`
- distroless base: no shell, no package manager
- runs as **non-root**, works under a **read-only root filesystem**
- health check is a flag on the binary (`--health`), so the image needs no curl

```bash
docker run --read-only --cap-drop ALL -p 8080:8080 renatex314/netcluster-server
```

## Sizing

| devices | index memory | container limit |
|---|---|---|
| 100,000 | 28 MB | 256Mi |
| 500,000 | 115 MB | 768Mi |
| 1,000,000 | 180 MB | 1Gi |

Ask for roughly triple the steady state: the index grows by doubling its arrays,
so peak RSS during a rebuild runs above the settled figure.

**There is nothing to persist.** No volume, no PVC, no StatefulSet, no backup. The
index holds no truth — the authority for where your devices are is whatever
produces the position reports, and this is a materialised view of that stream. A
container that dies is a container you restart, and it refills at about a
microsecond per device.

## Scaling

Every replica holds the **complete** index, so the scaling model is replication,
not sharding: run N processes, feed them all the same stream, query any of them.

Two rules, and both bite late if you miss them:

- **Writes must not go through a round-robin load balancer.** Each replica needs
  every report, or its map is wrong. Address replicas individually and fan out;
  the Node client does this when given `urls: [...]`.
- **Do not shard geographically.** This hierarchy is globally coupled at coarse
  zooms — a cluster at `z=0` spans continents — so splitting the world makes the
  coarse zooms wrong. Shard by collection instead.

MIT licensed.
