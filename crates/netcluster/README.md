# netcluster

Fully dynamic hierarchical geospatial clustering. Like [supercluster], except you
can **move a point without rebuilding the index**.

```text
supercluster:  one device moved  ->  reload 500k points  ->  ~870 ms
netcluster:    one device moved  ->  ~0.65 us
```

Supercluster builds a static KD-tree per zoom level. It is excellent, and it is
immutable by design — [the request for incremental updates has been open since
2016][issue19]. If your points move (vehicles, deliveries, people) you either
rebuild the whole index on a timer or you fall back to fixed-grid bucketing and
accept the boundary artifacts.

This crate maintains a **hierarchy of nets** — a cover tree over Web Mercator —
whose invariants are repaired locally on every update. Nothing is ever recomputed
globally, and there is no periodic rebuild.

## Measured

`cargo run --release --example bench`, 500,000 points in a Brazilian-fleet
distribution, Apple M-series. The JavaScript column is [netcluster-js] measured on
the same machine with `bench/ops.js`, and memory is peak RSS of the whole process
including runtime.

| | supercluster | netcluster-js | **netcluster (Rust)** |
|---|---|---|---|
| insert one point | 872,000 µs (full reload) | 2.10 µs | **0.86 µs** |
| move one point | 872,000 µs (full reload) | 2.13 µs | **0.65 µs** |
| remove one point | 872,000 µs (full reload) | 7.94 µs | **1.76 µs** |
| peak RSS @ 500k | 551 MB | 244 MB | **172 MB** |

Queries are `O(K)` in the number of markers returned, not in `N`:

| zoom | markers | query |
|---|---|---|
| 4 | 10 | 0.00 ms |
| 8 | 469 | 0.01 ms |
| 12 | 85,100 | 2.57 ms |

97% of realistic (~12 m) position reports take the fast path, where nothing but
aggregates and possibly one grid cell changes.

## Use

```toml
[dependencies]
netcluster = "0.1"
```

```rust
use netcluster::{NetCluster, Options, Feature};

let mut nc = NetCluster::new(Options::default());

nc.insert(1, -46.6333, -23.5505);
nc.insert(2, -46.6340, -23.5510);
nc.insert(3, -43.1729, -22.9068);

for f in nc.get_clusters([-80.0, -40.0, -30.0, 10.0], 4.0, -1) {
    match f {
        Feature::Cluster { count, lng, lat, .. } => println!("{count} vehicles at {lng}, {lat}"),
        Feature::Point { id, lng, lat }         => println!("vehicle {id} at {lng}, {lat}"),
    }
}

// The whole point: this is microseconds, not a rebuild.
nc.move_to(1, -46.6400, -23.5600);
```

Queries take `&self` and mutations take `&mut self`, so `RwLock<NetCluster>` gives
you concurrent readers against one writer with nothing else to build.

### Filtering

Declaring categories up front lets *"only the vehicles whose status is 3"* be
answered from precomputed aggregates rather than by scanning:

```rust
let mut nc = NetCluster::new(Options { categories: 8, ..Default::default() });
nc.insert_with_category(1, -46.63, -23.55, 3);

let only_status_3 = nc.get_clusters(bbox, 12.0, 3);
```

Each node carries `K` slices instead of one, but a point belongs to exactly one
category, so an update still touches exactly one slice per level — **update cost
does not grow with K**.

## How it works

For every zoom level `z`, the index maintains a *net* of the live point set at
scale `r_z = radius / (extent · 2^z)`:

- **Nesting** — `C_0 ⊆ C_1 ⊆ … ⊆ C_maxZoom ⊆ P`
- **Separation** — distinct centers of `C_z` are more than `r_z` apart
- **Covering** — every `p ∈ C_{z+1} \ C_z` has a parent `q ∈ C_z` with `d(p,q) ≤ r_z`

Those are the invariants of a compressed net-tree, equivalently a cover tree, over
the Web-Mercator plane. Two guarantees follow and hold at every level at every
moment:

- **radius** — `d(p, rep_z(p)) ≤ Σ_{j≥z} r_j ≤ 2·r_z`, by a geometric series
- **count** — `|C_z| ≤ |OPT(r_z/2)|`, because an `r_z`-separated set puts at most
  one member in any ball of radius `r_z/2`

so the level-`z` clustering is permanently a bicriteria (2, 1)-approximation of the
optimal radius-`r_z` clustering — without any global recomputation.

The reframing that makes it work: *fixed-k* dynamic clustering is genuinely hard,
but *fixed-radius-per-zoom* clustering is easy, and a map's zoom levels already
**are** the geometric scale ladder a net-tree needs.

Everything is exact integer arithmetic over fixed-point Web Mercator (`2^30` units,
about 3.7 cm at the equator), so a cluster centroid does not drift no matter how
long the index runs.

## Correctness

```
cargo test --release
```

- **`verify()`** re-derives every invariant from scratch — subtree sums by walking
  the tree, separation pairwise, grid membership from raw coordinates — consulting
  none of the index's own bookkeeping. Six unit tests corrupt the structure
  deliberately to prove the checker actually fails when it should.
- **`tests/invariants.rs`** runs long randomised insert/move/remove sequences under
  five geometries, re-verifying at checkpoints, including a duplicate-coordinate
  torture case where every distance is zero.
- **`tests/differential.rs`** replays 15,680 operations recorded from the
  JavaScript implementation and compares the **complete device-to-representative
  map at every zoom** at 79 checkpoints. Not cluster counts, not centroids: *who is
  grouped with whom*.

That last one earns its keep. Distance ties are broken by the lexicographic order
of the ids' **decimal renderings** — so `10` sorts before `9` — which looks like a
bug and is not: it is what makes the tree a function of the point set and the
operation order alone, never of hash or chain iteration order. Substituting the
"obvious" numeric comparison still produces the right *number* of clusters and
still passes every structural test, but silently puts devices in different
clusters. The differential test catches it at the second checkpoint.

## Porting notes

A faithful port of [netcluster-js], with four deliberate differences:

| | JavaScript | Rust |
|---|---|---|
| ids | any number | `u64` — intern strings yourself |
| payloads | stored in the index | keep them beside it; the index carries ids and categories |
| coordinate sums | `f64`, exact to ~8M points | `i64`, exact to `2^33` points |
| distances | `f64` | `i64`, exact everywhere |

The tie-break rule is preserved exactly: for non-negative integers below 1e21,
JavaScript's `String` is the plain decimal expansion, so `cmp_decimal` agrees with
it bit for bit. That is what makes the differential test possible.

`insert_projected` and `move_to_projected` take pre-projected fixed-point
coordinates. Use them when comparing implementations — `sin`, `ln` and `atan` are
not bit-identical across libms, and a harness that projects independently on each
side will eventually diverge for reasons that have nothing to do with the
algorithm.

## License

MIT

[supercluster]: https://github.com/mapbox/supercluster
[netcluster-js]: https://github.com/renatex314/NetCluster
[issue19]: https://github.com/mapbox/supercluster/issues/19
