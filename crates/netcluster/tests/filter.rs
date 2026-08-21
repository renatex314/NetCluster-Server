//! Filtered queries: "show me only the vehicles whose status is 3".
//!
//! The point of the design is that this costs nothing extra at query time and
//! nothing extra at update time. Each node carries K aggregate slices instead of
//! one, but a point belongs to exactly one category, so an update still touches
//! exactly one slice per level -- the update cost does not grow with K. These
//! tests check the answers are right; `bench/` checks the cost claim.

use netcluster::{project, Feature, NetCluster, Options};
use std::collections::HashMap;

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f64 {
        let mut s = self.0;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.0 = s;
        s as f64 / 4_294_967_296.0
    }
}

const WORLD: [f64; 4] = [-180.0, -85.0, 180.0, 85.0];
const K: usize = 5;

struct World {
    nc: NetCluster,
    pos: HashMap<u64, (i32, i32)>,
    cat: HashMap<u64, u32>,
}

fn build(n: u64, seed: u32, k: usize) -> World {
    let mut rng = Rng(seed);
    let mut nc = NetCluster::new(Options {
        categories: k,
        ..Default::default()
    });
    let cities: &[(f64, f64)] = &[(-46.63, -23.55), (2.35, 48.85), (-74.0, 40.71)];
    let mut pos = HashMap::new();
    let mut cat = HashMap::new();
    for i in 0..n {
        let c = cities[(rng.next() * 3.0) as usize % 3];
        let (lng, lat) = (
            c.0 + (rng.next() - 0.5) * 0.6,
            c.1 + (rng.next() - 0.5) * 0.6,
        );
        let k = (rng.next() * k as f64) as u32 % k as u32;
        nc.insert_with_category(i, lng, lat, k);
        pos.insert(i, project(lng, lat));
        cat.insert(i, k);
    }
    World { nc, pos, cat }
}

/// Brute force: group the points of one category by their representative at zoom
/// `z`, and take each group's exact centroid. That is what a filtered query must
/// return -- no more, no less.
fn expected(w: &World, z: i32, c: u32) -> Vec<(u32, f64, f64)> {
    let mut groups: HashMap<u64, (u32, i64, i64)> = HashMap::new();
    for (&id, &cat) in &w.cat {
        if cat != c {
            continue;
        }
        let rep = w.nc.representative(id, z).unwrap();
        let p = w.pos[&id];
        let e = groups.entry(rep).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += p.0 as i64;
        e.2 += p.1 as i64;
    }
    let mut out: Vec<(u32, f64, f64)> = groups
        .values()
        .map(|&(n, sx, sy)| {
            let (lng, lat) = netcluster::unproject(sx as f64 / n as f64, sy as f64 / n as f64);
            (n, lng, lat)
        })
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

fn actual(w: &World, z: i32, c: u32) -> Vec<(u32, f64, f64)> {
    let mut out: Vec<(u32, f64, f64)> =
        w.nc.get_clusters(WORLD, z as f64, c as i32)
            .iter()
            .map(|f| (f.count(), f.lng(), f.lat()))
            .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

#[test]
fn a_filtered_query_matches_brute_force_at_every_zoom() {
    let w = build(700, 5150, K);
    for z in 0..=w.nc.max_zoom() as i32 {
        for c in 0..K as u32 {
            let want = expected(&w, z, c);
            let got = actual(&w, z, c);
            assert_eq!(
                got.len(),
                want.len(),
                "z={z} cat={c}: {} clusters, brute force says {}",
                got.len(),
                want.len()
            );
            for (a, b) in got.iter().zip(want.iter()) {
                assert_eq!(a.0, b.0, "z={z} cat={c}: count");
                assert!(
                    (a.1 - b.1).abs() < 1e-9,
                    "z={z} cat={c}: lng {} vs {}",
                    a.1,
                    b.1
                );
                assert!(
                    (a.2 - b.2).abs() < 1e-9,
                    "z={z} cat={c}: lat {} vs {}",
                    a.2,
                    b.2
                );
            }
        }
    }
}

/// The unfiltered query must equal the sum of the filtered ones, point for point.
#[test]
fn the_categories_partition_the_whole_set() {
    let w = build(700, 5150, K);
    for z in [0, 3, 7, 12, 16] {
        let all: u32 =
            w.nc.get_clusters(WORLD, z as f64, -1)
                .iter()
                .map(|f| f.count())
                .sum();
        assert_eq!(all, 700);
        let summed: u32 = (0..K as u32)
            .map(|c| {
                w.nc.get_clusters(WORLD, z as f64, c as i32)
                    .iter()
                    .map(|f| f.count())
                    .sum::<u32>()
            })
            .sum();
        assert_eq!(summed, 700, "z={z}: filtered queries summed to {summed}");
    }
}

/// When a filter leaves exactly one point in a cluster, the query must return
/// *that point's* identity and position -- not the cluster centre it happens to
/// sit under. Getting this wrong puts the marker in the wrong place, which is the
/// kind of bug nobody notices until a driver is dispatched to it.
#[test]
fn a_filtered_singleton_resolves_to_the_actual_point() {
    let w = build(700, 5150, K);
    let mut singles = 0;
    for z in 0..=w.nc.max_zoom() as i32 {
        for c in 0..K as u32 {
            for f in w.nc.get_clusters(WORLD, z as f64, c as i32) {
                let Feature::Point { id, lng, lat } = f else {
                    continue;
                };
                assert_eq!(
                    w.cat[&id], c,
                    "z={z}: singleton {id} is category {} not {c}",
                    w.cat[&id]
                );
                let p = w.pos[&id];
                let (elng, elat) = netcluster::unproject(p.0 as f64, p.1 as f64);
                assert!(
                    (lng - elng).abs() < 1e-9 && (lat - elat).abs() < 1e-9,
                    "z={z} cat={c}: singleton {id} drawn at the wrong place"
                );
                singles += 1;
            }
        }
    }
    assert!(
        singles > 100,
        "only {singles} filtered singletons exercised"
    );
}

/// A very sparse filter is the case people worry about. It must still be correct,
/// and the query must still be proportional to what it returns, not to N.
#[test]
fn a_sparse_filter_stays_correct() {
    let mut rng = Rng(1234);
    let mut nc = NetCluster::new(Options {
        categories: 100,
        ..Default::default()
    });
    let mut pos = HashMap::new();
    let mut cat = HashMap::new();
    for i in 0..2000u64 {
        let (lng, lat) = (
            -46.63 + (rng.next() - 0.5) * 0.6,
            -23.55 + (rng.next() - 0.5) * 0.6,
        );
        // category 7 gets 1% of the points; everything else is noise
        let c = if rng.next() < 0.01 {
            7
        } else {
            1 + (rng.next() * 90.0) as u32 % 90
        };
        nc.insert_with_category(i, lng, lat, c);
        pos.insert(i, project(lng, lat));
        cat.insert(i, c);
    }
    nc.verify().unwrap();
    let want = cat.values().filter(|&&c| c == 7).count() as u32;
    assert!(want > 5, "sparse category ended up with {want} points");
    for z in 0..=nc.max_zoom() as i32 {
        let got: u32 = nc
            .get_clusters(WORLD, z as f64, 7)
            .iter()
            .map(|f| f.count())
            .sum();
        assert_eq!(
            got, want,
            "z={z}: sparse filter returned {got} of {want} points"
        );
    }
    // a category with no members at all must return nothing, not everything
    assert!(nc.get_clusters(WORLD, 5.0, 0).is_empty());
}

/// Moves and removals must keep the slices in step with the totals. `verify`
/// checks that directly; this drives enough churn to make it meaningful.
#[test]
fn slices_survive_churn() {
    let mut rng = Rng(2718);
    let mut w = build(300, 99, K);
    let mut next = 300u64;
    for step in 0..1500 {
        let u = rng.next();
        let ids: Vec<u64> = w.pos.keys().copied().collect();
        if u < 0.6 && !ids.is_empty() {
            let id = ids[(rng.next() * ids.len() as f64) as usize % ids.len()];
            let (lng, lat) = (
                -46.63 + (rng.next() - 0.5) * 0.8,
                -23.55 + (rng.next() - 0.5) * 0.8,
            );
            w.nc.move_to(id, lng, lat);
            w.pos.insert(id, project(lng, lat));
        } else if u < 0.8 {
            let (lng, lat) = (
                2.35 + (rng.next() - 0.5) * 0.8,
                48.85 + (rng.next() - 0.5) * 0.8,
            );
            let c = (rng.next() * K as f64) as u32 % K as u32;
            w.nc.insert_with_category(next, lng, lat, c);
            w.pos.insert(next, project(lng, lat));
            w.cat.insert(next, c);
            next += 1;
        } else if !ids.is_empty() {
            let id = ids[(rng.next() * ids.len() as f64) as usize % ids.len()];
            w.nc.remove(id);
            w.pos.remove(&id);
            w.cat.remove(&id);
        }
        if step % 300 == 0 {
            w.nc.verify().unwrap_or_else(|e| panic!("step {step}: {e}"));
        }
    }
    w.nc.verify().unwrap();
    // and the answers are still right after all that
    for z in [0, 5, 11, 16] {
        for c in 0..K as u32 {
            assert_eq!(
                actual(&w, z, c),
                expected(&w, z, c),
                "z={z} cat={c} after churn"
            );
        }
    }
}
