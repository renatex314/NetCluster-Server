//! cargo run --release --example bench [N]
//!
//! Measures the operations a real fleet performs: build once, then move
//! continuously while queries run against the same index.

use netcluster::{NetCluster, Options, PREC_I};
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Vehicles are not uniformly scattered; they pile up in cities. A uniform
/// benchmark would flatter the structure by keeping every cell sparse.
const HUBS: &[(f64, f64)] = &[
    (-46.63, -23.55),
    (-43.17, -22.90),
    (-47.88, -15.79),
    (-38.52, -3.73),
    (-34.88, -8.05),
    (-51.23, -30.03),
    (-49.27, -25.43),
    (-35.73, -9.66),
    (-60.02, -3.10),
    (-48.50, -1.45),
];

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000);

    let mut rng = Rng(0x5EED);
    let mut nc = NetCluster::new(Options::default());

    // ---- build
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(n);
    for _ in 0..n {
        let h = HUBS[(rng.next() * HUBS.len() as f64) as usize % HUBS.len()];
        pts.push((
            h.0 + (rng.next() - 0.5) * 0.9,
            h.1 + (rng.next() - 0.5) * 0.9,
        ));
    }
    let t = Instant::now();
    for (i, p) in pts.iter().enumerate() {
        nc.insert(i as u64, p.0, p.1);
    }
    let build = t.elapsed();
    println!("netcluster  N = {n}");
    println!(
        "  build          {:>8.2} s   ({:.0} points/s)",
        build.as_secs_f64(),
        n as f64 / build.as_secs_f64()
    );
    println!(
        "  memory         {:>8.1} MB  ({:.0} bytes/point, {:.1} grid listings/point)",
        nc.memory_bytes() as f64 / 1e6,
        nc.memory_bytes() as f64 / n as f64,
        nc.grid_entries() as f64 / n as f64
    );

    // ---- move: realistic creep, roughly 12 m per report
    // 1 fixed-point unit is about 3.7 cm at the equator, so 12 m is ~320 units.
    let moves = (n).min(500_000);
    let mut xs: Vec<(i32, i32)> = (0..n as u64).map(|i| nc.position(i).unwrap()).collect();
    let t = Instant::now();
    for k in 0..moves {
        let i = k % n;
        let (x, y) = xs[i];
        let nx = (x + ((rng.next() - 0.5) * 640.0) as i32).clamp(0, PREC_I as i32);
        let ny = (y + ((rng.next() - 0.5) * 640.0) as i32).clamp(0, PREC_I as i32);
        nc.move_to_projected(i as u64, nx, ny);
        xs[i] = (nx, ny);
    }
    let mv = t.elapsed();
    let st = nc.stats;
    println!(
        "  move (12 m)    {:>8.2} us/op ({:.0} moves/s, {:.1}% fast path)",
        mv.as_secs_f64() * 1e6 / moves as f64,
        moves as f64 / mv.as_secs_f64(),
        100.0 * st.moves_fast as f64 / st.moves as f64
    );

    // ---- move: teleport, the worst case
    let tp = (n / 20).max(1000);
    let t = Instant::now();
    for k in 0..tp {
        let i = k % n;
        let h = HUBS[(rng.next() * HUBS.len() as f64) as usize % HUBS.len()];
        nc.move_to(
            i as u64,
            h.0 + (rng.next() - 0.5) * 0.9,
            h.1 + (rng.next() - 0.5) * 0.9,
        );
    }
    println!(
        "  move (teleport){:>8.2} us/op",
        t.elapsed().as_secs_f64() * 1e6 / tp as f64
    );

    // ---- query
    println!("  query (whole of Brazil):");
    let bbox = [-74.0, -34.0, -34.0, 6.0];
    for z in [0, 4, 8, 12, 16] {
        let t = Instant::now();
        let reps = 20;
        let mut count = 0;
        for _ in 0..reps {
            count = nc.get_clusters(bbox, z as f64, -1).len();
        }
        println!(
            "    z={z:<2}         {:>8.2} ms   ({count} markers)",
            t.elapsed().as_secs_f64() * 1e3 / reps as f64
        );
    }

    // ---- remove
    let rm = (n / 10).max(1000);
    let t = Instant::now();
    for i in 0..rm {
        nc.remove(i as u64);
    }
    println!(
        "  remove         {:>8.2} us/op",
        t.elapsed().as_secs_f64() * 1e6 / rm as f64
    );
    println!("  {} points left", nc.len());
}
