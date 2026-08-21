//! Long randomised sequences of insert / move / remove, with every structural
//! invariant re-verified from scratch at checkpoints along the way.
//!
//! These mirror the JavaScript suite scenario for scenario, including the same
//! seeds, so a failure here is comparable to a failure there.

use netcluster::{NetCluster, Options};

/// xorshift32, matching the generator the JavaScript tests use, so that a
/// scenario with the same seed explores the same shape of state space.
struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Self {
        Rng(seed)
    }
    fn next(&mut self) -> f64 {
        let mut s = self.0;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.0 = s;
        s as f64 / 4_294_967_296.0
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() * xs.len() as f64) as usize % xs.len()]
    }
}

const CITIES: &[(f64, f64)] = &[
    (-46.63, -23.55),
    (2.35, 48.85),
    (-74.0, 40.71),
    (139.69, 35.68),
    (116.4, 39.9),
    (-43.17, -22.9),
    (13.4, 52.52),
    (151.2, -33.87),
    (55.27, 25.2),
    (-99.13, 19.43),
];

fn check(nc: &NetCluster, label: &str) {
    match nc.verify() {
        Ok(_) => {}
        Err(e) => panic!("[{label}] {e}"),
    }
}

/// Dense city blobs plus a uniform spread. The mixture is the point: a net is
/// easiest on uniform data and hardest where density changes sharply.
fn pick_point(rng: &mut Rng, world: &[(f64, f64)]) -> (f64, f64) {
    if rng.next() < 0.7 {
        let c = rng.pick(world);
        (
            c.0 + (rng.next() - 0.5) * 0.4,
            c.1 + (rng.next() - 0.5) * 0.4,
        )
    } else {
        (rng.next() * 360.0 - 180.0, rng.next() * 140.0 - 70.0)
    }
}

fn run(name: &str, seed: u32, n: u64, steps: usize, opts: Options, world: &[(f64, f64)]) {
    let mut rng = Rng::new(seed);
    let mut nc = NetCluster::new(opts);
    let mut pos: Vec<(u64, (f64, f64))> = Vec::new();

    for i in 0..n {
        let p = pick_point(&mut rng, world);
        nc.insert(i, p.0, p.1);
        pos.push((i, p));
    }
    check(&nc, &format!("{name}:build"));

    let mut next_id = n;
    for step in 0..steps {
        let u = rng.next();
        if u < 0.55 && !pos.is_empty() {
            let i = (rng.next() * pos.len() as f64) as usize % pos.len();
            let (id, p) = pos[i];
            let q = if rng.next() < 0.85 {
                // creep: the realistic case, and the one the fast path exists for
                (
                    p.0 + (rng.next() - 0.5) * 0.02,
                    p.1 + (rng.next() - 0.5) * 0.02,
                )
            } else {
                // teleport: forces the repair path
                pick_point(&mut rng, world)
            };
            nc.move_to(id, q.0, q.1);
            pos[i] = (id, q);
        } else if u < 0.78 {
            let p = pick_point(&mut rng, world);
            nc.insert(next_id, p.0, p.1);
            pos.push((next_id, p));
            next_id += 1;
        } else if !pos.is_empty() {
            let i = (rng.next() * pos.len() as f64) as usize % pos.len();
            let (id, _) = pos.swap_remove(i);
            assert!(nc.remove(id), "remove of live id {id} reported missing");
        }
        if step % 200 == 0 {
            check(&nc, &format!("{name}:step{step}"));
        }
    }
    check(&nc, &format!("{name}:final"));

    // Duplicate-coordinate torture: many points at the exact same spot. Every
    // distance is zero, so the tie-break rule is the only thing keeping the
    // structure well defined.
    for i in 0..60u64 {
        nc.insert(1_000_000 + i, 10.0, 10.0);
    }
    check(&nc, &format!("{name}:dupes"));
    for i in 0..30u64 {
        assert!(nc.remove(1_000_000 + i));
    }
    check(&nc, &format!("{name}:dupes-removed"));
}

#[test]
fn mixed_z16() {
    run("mixed-z16", 12345, 400, 3000, Options::default(), CITIES);
}

#[test]
fn shallow_z6() {
    run(
        "shallow-z6",
        999,
        300,
        2000,
        Options {
            max_zoom: 6,
            ..Default::default()
        },
        CITIES,
    );
}

/// hysteresis = 0 is the textbook net: no slack at all, so every marginal move
/// triggers a repair. It is the strictest setting the invariants can be checked at.
#[test]
fn no_hysteresis() {
    run(
        "nohyst",
        777,
        300,
        2000,
        Options {
            max_zoom: 14,
            hysteresis: 0.0,
            ..Default::default()
        },
        CITIES,
    );
}

/// hysteresis = 1.0 doubles the permitted covering distance. The radius bound has
/// to stretch with it, which is exactly what verify() asserts.
#[test]
fn big_hysteresis() {
    run(
        "bighyst",
        31337,
        250,
        1500,
        Options {
            max_zoom: 12,
            hysteresis: 1.0,
            ..Default::default()
        },
        CITIES,
    );
}

#[test]
fn single_city() {
    run(
        "single-city",
        4242,
        500,
        2500,
        Options::default(),
        &[(-46.63, -23.55)],
    );
}

/// Point spacing well below r_maxZoom, so most points are never a center at any
/// level. Exercises the leaf fast path, which is the common case for a real fleet.
#[test]
fn dense_sub_radius_spacing() {
    let mut rng = Rng::new(2024);
    let mut nc = NetCluster::new(Options::default());
    let c = (-46.6333, -23.5505);
    let s = 0.004; // roughly a 450 m box
    let mut pos: Vec<(u64, (f64, f64))> = Vec::new();
    let pick = |rng: &mut Rng| (c.0 + (rng.next() - 0.5) * s, c.1 + (rng.next() - 0.5) * s);
    for i in 0..500u64 {
        let p = pick(&mut rng);
        nc.insert(i, p.0, p.1);
        pos.push((i, p));
    }
    check(&nc, "dense:build");
    let mut next_id = 500u64;
    for step in 0..2000 {
        let u = rng.next();
        if u < 0.6 && !pos.is_empty() {
            let i = (rng.next() * pos.len() as f64) as usize % pos.len();
            let (id, p) = pos[i];
            let q = (
                p.0 + (rng.next() - 0.5) * 0.0004,
                p.1 + (rng.next() - 0.5) * 0.0004,
            );
            nc.move_to(id, q.0, q.1);
            pos[i] = (id, q);
        } else if u < 0.8 {
            let p = pick(&mut rng);
            nc.insert(next_id, p.0, p.1);
            pos.push((next_id, p));
            next_id += 1;
        } else if !pos.is_empty() {
            let i = (rng.next() * pos.len() as f64) as usize % pos.len();
            let (id, _) = pos.swap_remove(i);
            nc.remove(id);
        }
        if step % 200 == 0 {
            check(&nc, &format!("dense:step{step}"));
        }
    }
    let v = nc.verify().expect("dense:final");
    assert!(
        v.leaves > v.centers,
        "sub-radius spacing should leave most points as leaves, got {} leaves / {} centers",
        v.leaves,
        v.centers
    );
    // The fast path is the entire reason this is cheap; if it stops firing on
    // creeping points, the port has regressed even though every invariant holds.
    let st = nc.stats;
    let fast_pct = 100.0 * st.moves_fast as f64 / st.moves as f64;
    assert!(
        fast_pct > 80.0,
        "only {fast_pct:.0}% of creeping moves took the fast path"
    );
}

/// Every point inserted must be findable, and removal must be complete: no
/// residue in the grid, no residue in the aggregates.
#[test]
fn full_drain_leaves_nothing_behind() {
    let mut rng = Rng::new(8080);
    let mut nc = NetCluster::new(Options::default());
    let mut ids = Vec::new();
    for i in 0..300u64 {
        let p = pick_point(&mut rng, CITIES);
        nc.insert(i, p.0, p.1);
        ids.push(i);
    }
    check(&nc, "drain:build");
    while let Some(id) = ids.pop() {
        assert!(nc.remove(id));
    }
    let v = nc.verify().expect("drain:empty");
    assert_eq!(v.points, 0);
    assert_eq!(v.grid_listings, 0, "grid entries leaked after draining");
    assert_eq!(nc.grid_entries(), 0);
    assert!(nc.is_empty());
    // and it must still work afterwards
    nc.insert(1, 0.0, 0.0);
    check(&nc, "drain:reuse");
}

/// A sanity check on the checks: confirm the stress scenarios actually build a
/// non-trivial hierarchy, so that a silently-degenerate RNG cannot make the whole
/// suite pass by exercising nothing.
#[test]
fn scenarios_are_not_degenerate() {
    let mut rng = Rng::new(12345);
    let mut nc = NetCluster::new(Options::default());
    for i in 0..400u64 {
        let p = pick_point(&mut rng, CITIES);
        nc.insert(i, p.0, p.1);
    }
    let v = nc.verify().unwrap();
    assert_eq!(v.points, 400);
    assert!(
        v.centers > 20,
        "only {} centers -- points are not spread",
        v.centers
    );
    assert!(v.max_depth >= 3, "tree is only {} deep", v.max_depth);
    assert!(
        v.grid_listings > 400,
        "only {} grid listings for 400 points",
        v.grid_listings
    );
    let c = nc.get_clusters([-180.0, -85.0, 180.0, 85.0], 3.0, -1);
    assert!(
        c.len() > 3 && c.len() < 400,
        "world at z3 gave {} features",
        c.len()
    );
    let total: u32 = c.iter().map(|f| f.count()).sum();
    assert_eq!(
        total, 400,
        "clusters must account for every point exactly once"
    );
    eprintln!(
        "  build: {} points, {} centers, {} leaves, depth {}, {} grid listings, {} features at z3",
        v.points,
        v.centers,
        v.leaves,
        v.max_depth,
        v.grid_listings,
        c.len()
    );
}
