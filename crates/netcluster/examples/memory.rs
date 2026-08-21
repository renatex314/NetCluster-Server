//! cargo run --release --example memory [N]
//!
//! Builds one index and nothing else, then reports peak RSS. Measuring an index
//! this way -- one per process, no scratch data alive alongside it -- is the only
//! honest way to do it: same-process heap deltas are dominated by allocator and
//! GC noise, which is how the JavaScript build once managed to report *negative*
//! memory use for an index holding a million points.

use netcluster::{NetCluster, Options};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

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
        .unwrap_or(1_000_000);
    let mut rng = Rng(0x5EED);
    let mut nc = NetCluster::new(Options::default());
    // Generated streaming, so nothing but the index is alive at the peak.
    for i in 0..n {
        let h = HUBS[(rng.next() * HUBS.len() as f64) as usize % HUBS.len()];
        nc.insert(
            i as u64,
            h.0 + (rng.next() - 0.5) * 0.9,
            h.1 + (rng.next() - 0.5) * 0.9,
        );
    }
    println!(
        "{n} points   estimate {:.1} MB   ({:.0} bytes/point, {:.2} grid listings/point)",
        nc.memory_bytes() as f64 / 1e6,
        nc.memory_bytes() as f64 / n as f64,
        nc.grid_entries() as f64 / n as f64
    );
    println!("run under `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) for true RSS");
    // keep it alive past the measurement
    std::hint::black_box(&nc);
}
