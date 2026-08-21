//! The read API: partitions, expansion, children, leaves, viewport agreement,
//! and the handling of handles that no longer mean anything.

use netcluster::{Feature, NetCluster, Options};
use std::collections::{HashMap, HashSet};

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

fn build(n: u64, seed: u32) -> NetCluster {
    let mut rng = Rng(seed);
    let mut nc = NetCluster::new(Options::default());
    let cities: &[(f64, f64)] = &[
        (-46.63, -23.55),
        (2.35, 48.85),
        (-74.0, 40.71),
        (139.69, 35.68),
    ];
    for i in 0..n {
        let c = cities[(rng.next() * 4.0) as usize % 4];
        nc.insert(
            i,
            c.0 + (rng.next() - 0.5) * 0.5,
            c.1 + (rng.next() - 0.5) * 0.5,
        );
    }
    nc
}

/// At every zoom, the returned features must account for every point exactly
/// once. This is the property a map actually depends on: no vanished vehicles,
/// no double-counted ones.
#[test]
fn every_zoom_is_a_partition() {
    let nc = build(600, 4242);
    for z in 0..=nc.max_zoom() as i32 {
        let f = nc.get_clusters(WORLD, z as f64, -1);
        let total: u32 = f.iter().map(|x| x.count()).sum();
        assert_eq!(total, 600, "z={z} accounted for {total} of 600 points");
        assert!(!f.is_empty());
    }
    // and the extremes behave
    assert!(
        nc.get_clusters(WORLD, 0.0, -1).len() <= nc.get_clusters(WORLD, 16.0, -1).len(),
        "zooming in must not reduce the number of markers"
    );
    assert_eq!(
        nc.get_clusters(WORLD, 16.0, -1).len(),
        600,
        "at max zoom every point stands alone"
    );
}

/// A cluster's expansion zoom must be the zoom at which it actually splits --
/// strictly more markers than the zoom just below it.
#[test]
fn expansion_zoom_is_where_the_cluster_actually_splits() {
    let nc = build(500, 77);
    let mut checked = 0;
    for z in 0..8 {
        for f in nc.get_clusters(WORLD, z as f64, -1) {
            let Feature::Cluster {
                cluster_id, count, ..
            } = f
            else {
                continue;
            };
            let nz = nc.get_cluster_expansion_zoom(cluster_id).unwrap();
            assert!(nz > z, "expansion zoom {nz} is not beyond {z}");
            let kids = nc.get_children(cluster_id).unwrap();
            assert!(
                kids.len() >= 2,
                "a cluster expanded into {} feature(s)",
                kids.len()
            );
            let kid_total: u32 = kids.iter().map(|k| k.count()).sum();
            assert_eq!(
                kid_total, count,
                "children lost points from cluster {cluster_id}"
            );
            checked += 1;
        }
    }
    assert!(checked > 20, "only {checked} clusters exercised");
}

/// Leaves are the individual points inside a cluster; there must be exactly
/// `count` of them, all distinct, and pagination must not lose or repeat any.
#[test]
fn leaves_enumerate_the_members_exactly_once() {
    let nc = build(400, 31337);
    let mut checked = 0;
    for f in nc.get_clusters(WORLD, 3.0, -1) {
        let Feature::Cluster {
            cluster_id, count, ..
        } = f
        else {
            continue;
        };
        let all = nc.get_leaves(cluster_id, usize::MAX, 0).unwrap();
        assert_eq!(all.len() as u32, count, "cluster {cluster_id} leaf count");
        let ids: HashSet<u64> = all
            .iter()
            .map(|l| match l {
                Feature::Point { id, .. } => *id,
                _ => panic!("a leaf must be a point"),
            })
            .collect();
        assert_eq!(ids.len() as u32, count, "duplicate leaves");

        // paginate in threes and reassemble
        let mut page = Vec::new();
        let mut off = 0;
        loop {
            let p = nc.get_leaves(cluster_id, 3, off).unwrap();
            if p.is_empty() {
                break;
            }
            page.extend(p.iter().map(|l| match l {
                Feature::Point { id, .. } => *id,
                _ => unreachable!(),
            }));
            off += 3;
            if off > count as usize + 10 {
                break;
            }
        }
        assert_eq!(
            page.iter().copied().collect::<HashSet<_>>(),
            ids,
            "paginated leaves of {cluster_id} differ from the full list"
        );
        checked += 1;
    }
    assert!(checked > 3, "only {checked} clusters exercised");
}

/// A viewport query must return exactly what a world query would, restricted to
/// the viewport. If these disagree, the map shows different things depending on
/// where you happen to have scrolled.
#[test]
fn a_viewport_query_agrees_with_the_world_query() {
    let nc = build(600, 555);
    let boxes: &[[f64; 4]] = &[
        [-47.5, -24.5, -45.5, -22.5],
        [1.0, 48.0, 4.0, 50.0],
        [-80.0, 30.0, -60.0, 50.0],
        [130.0, 30.0, 150.0, 40.0],
    ];
    for z in [2, 5, 8, 12] {
        let world = nc.get_clusters(WORLD, z as f64, -1);
        for b in boxes {
            let got = nc.get_clusters(*b, z as f64, -1);
            let want: Vec<&Feature> = world
                .iter()
                .filter(|f| {
                    f.lng() >= b[0] && f.lng() <= b[2] && f.lat() >= b[1] && f.lat() <= b[3]
                })
                .collect();
            assert_eq!(
                got.len(),
                want.len(),
                "z={z} box={b:?}: viewport gave {} features, world-then-filter gave {}",
                got.len(),
                want.len()
            );
        }
    }
}

/// A device's representative at zoom z must be the cluster that actually contains
/// it -- the link between "where is my vehicle" and "what is drawn".
#[test]
fn representative_points_at_the_cluster_that_contains_the_device() {
    let nc = build(300, 909);
    for z in [0, 4, 9, 16] {
        let mut members: HashMap<u64, Vec<u64>> = HashMap::new();
        for id in 0..300u64 {
            members
                .entry(nc.representative(id, z).unwrap())
                .or_default()
                .push(id);
        }
        let total: usize = members.values().map(|v| v.len()).sum();
        assert_eq!(total, 300);
        assert_eq!(
            members.len(),
            nc.get_clusters(WORLD, z as f64, -1).len(),
            "z={z}: representative groups disagree with the query"
        );
    }
    assert_eq!(nc.representative(999_999, 5), None, "unknown device");
}

/// A handle that does not name a live cluster must be rejected, not followed.
///
/// The JavaScript implementation once spun forever here: a bad value indexed past
/// the end of a typed array, yielded `undefined`, which never equalled the
/// sentinel, so the sibling walk never terminated. Rust's type system rules out
/// the original confusion -- a `u64` cannot be the string `"vehicle-1"` -- but a
/// stale or out-of-range handle is still perfectly possible.
#[test]
fn stale_cluster_handles_are_rejected() {
    let mut nc = build(200, 12);
    let cluster = nc
        .get_clusters(WORLD, 2.0, -1)
        .into_iter()
        .find_map(|f| match f {
            Feature::Cluster { cluster_id, .. } => Some(cluster_id),
            _ => None,
        })
        .expect("expected at least one cluster");

    assert!(nc.get_children(cluster).is_ok());

    for bogus in [u64::MAX, u64::MAX / 2, 1 << 40, 999_999_999] {
        assert!(nc.get_children(bogus).is_err(), "{bogus} was accepted");
        assert!(nc.get_leaves(bogus, 10, 0).is_err(), "{bogus} was accepted");
        assert!(
            nc.get_cluster_expansion_zoom(bogus).is_err(),
            "{bogus} was accepted"
        );
    }

    // A handle whose slot has since been freed must not resolve either.
    for id in 0..200u64 {
        nc.remove(id);
    }
    assert!(
        nc.get_children(cluster).is_err(),
        "a handle into a drained index still resolved"
    );

    let msg = nc.get_children(u64::MAX).unwrap_err().to_string();
    assert!(msg.contains("cluster id"), "unhelpful error: {msg}");
    assert!(
        msg.contains("representative"),
        "error should point at the fix: {msg}"
    );
}

/// Tile queries must carry the same content as the equivalent bbox query, and pad
/// the seam so a marker on a tile boundary is not clipped in half.
#[test]
fn tiles_cover_the_world_without_losing_points() {
    let nc = build(400, 246);
    for z in 0..4 {
        let side = 1i64 << z;
        let mut seen_points = 0u32;
        for tx in 0..side {
            for ty in 0..side {
                for f in nc.get_tile(z, tx, ty, -1) {
                    // only count features whose centre is inside the tile proper,
                    // so the padding does not double-count
                    let e = 512;
                    if f.x >= 0 && f.x < e && f.y >= 0 && f.y < e {
                        seen_points += f.count;
                    }
                }
            }
        }
        assert_eq!(
            seen_points, 400,
            "z={z}: tiles accounted for {seen_points} of 400 points"
        );
    }
}

#[test]
fn an_empty_index_answers_queries_instead_of_panicking() {
    let nc = NetCluster::new(Options::default());
    assert!(nc.is_empty());
    assert_eq!(nc.get_clusters(WORLD, 5.0, -1).len(), 0);
    assert_eq!(nc.get_tile(0, 0, 0, -1).len(), 0);
    assert_eq!(nc.representative(1, 0), None);
    nc.verify().unwrap();
}

#[test]
fn reinserting_the_same_id_moves_it_rather_than_duplicating() {
    let mut nc = NetCluster::new(Options::default());
    nc.insert(1, -46.63, -23.55);
    nc.insert(1, 2.35, 48.85);
    assert_eq!(nc.len(), 1);
    let f = nc.get_clusters(WORLD, 16.0, -1);
    assert_eq!(f.len(), 1);
    assert!((f[0].lng() - 2.35).abs() < 1e-6, "id 1 did not move");
    nc.verify().unwrap();
}

/// `cluster_of` must name the same marker a viewport query would draw the device
/// inside of. If these disagree, "find my vehicle" highlights the wrong marker.
#[test]
fn cluster_of_agrees_with_the_viewport_query() {
    let nc = build(400, 616);
    for z in [0, 3, 7, 11, 16] {
        let drawn = nc.get_clusters(WORLD, z as f64, -1);
        for id in 0..400u64 {
            let c = nc
                .cluster_of(id, z)
                .expect("every live device has a cluster");
            let found = drawn.iter().find(|f| match (f, &c) {
                (
                    Feature::Cluster { cluster_id: a, .. },
                    Feature::Cluster { cluster_id: b, .. },
                ) => a == b,
                (Feature::Point { id: a, .. }, Feature::Point { id: b, .. }) => a == b,
                _ => false,
            });
            let f = found.unwrap_or_else(|| {
                panic!("z={z}: cluster_of({id}) is not among the drawn markers")
            });
            assert_eq!(f.count(), c.count(), "z={z}: device {id} count disagrees");
            assert!(
                (f.lng() - c.lng()).abs() < 1e-12,
                "z={z}: device {id} position disagrees"
            );
        }
    }
    assert_eq!(nc.cluster_of(999_999, 5), None);
}
