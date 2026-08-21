//! Differential test against the original JavaScript implementation.
//!
//! Every structural test in this crate checks that the Rust index is *internally*
//! consistent. This one checks something different and stronger: that it makes the
//! same decisions as the implementation it was ported from.
//!
//! The fixture is recorded by `tests/gen_fixture.mjs` running against the
//! JavaScript build. It carries the operation stream with coordinates already
//! projected -- `sin`, `ln` and `atan` are not bit-identical across libms, so
//! projecting independently on each side would eventually diverge for reasons
//! unrelated to the algorithm -- and, at each checkpoint, a hash of the complete
//! device-to-representative map at every zoom level.
//!
//! That map is the strongest available statement of agreement. Not cluster counts,
//! not centroids: *who is grouped with whom*, for every device, at every zoom.
//!
//! Regenerate with:
//!   cd crates/netcluster/tests && node gen_fixture.mjs > fixtures/differential.txt

use netcluster::{NetCluster, Options};
use std::collections::BTreeSet;

fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    h
}

#[test]
fn matches_the_javascript_implementation_device_for_device() {
    let src = include_str!("fixtures/differential.txt");

    let mut nc: Option<NetCluster> = None;
    let mut live: BTreeSet<u64> = BTreeSet::new();
    let mut scenario = String::from("<none>");
    let mut checkpoints = 0usize;
    let mut ops = 0usize;
    let mut scenarios = 0usize;
    let mut buf = String::with_capacity(1 << 16);

    for (lineno, line) in src.lines().enumerate() {
        let ln = lineno + 1;
        if line.is_empty() {
            continue;
        }
        let mut f = line.split(' ');
        let kind = f.next().unwrap();
        match kind {
            "#" => scenario = line.trim_start_matches("# scenario ").to_string(),
            "opts" => {
                let v: Vec<&str> = f.collect();
                assert_eq!(v.len(), 6, "line {ln}: malformed opts");
                nc = Some(NetCluster::new(Options {
                    min_zoom: v[0].parse().unwrap(),
                    max_zoom: v[1].parse().unwrap(),
                    radius: v[2].parse().unwrap(),
                    extent: v[3].parse().unwrap(),
                    hysteresis: v[4].parse().unwrap(),
                    categories: v[5].parse().unwrap(),
                }));
                live.clear();
                scenarios += 1;
            }
            "i" => {
                let idx = nc.as_mut().expect("op before opts");
                let id: u64 = f.next().unwrap().parse().unwrap();
                let x: i32 = f.next().unwrap().parse().unwrap();
                let y: i32 = f.next().unwrap().parse().unwrap();
                let c: u32 = f.next().unwrap().parse().unwrap();
                let c = if idx.categories() == 0 { 0 } else { c };
                idx.insert_projected(id, x, y, c);
                live.insert(id);
                ops += 1;
            }
            "m" => {
                let idx = nc.as_mut().expect("op before opts");
                let id: u64 = f.next().unwrap().parse().unwrap();
                let x: i32 = f.next().unwrap().parse().unwrap();
                let y: i32 = f.next().unwrap().parse().unwrap();
                idx.move_to_projected(id, x, y);
                ops += 1;
            }
            "r" => {
                let idx = nc.as_mut().expect("op before opts");
                let id: u64 = f.next().unwrap().parse().unwrap();
                idx.remove(id);
                live.remove(&id);
                ops += 1;
            }
            "c" => {
                let idx = nc.as_ref().expect("checkpoint before opts");
                let n: usize = f.next().unwrap().parse().unwrap();
                assert_eq!(
                    live.len(),
                    n,
                    "[{scenario}] line {ln}: live count {} != recorded {n}",
                    live.len()
                );
                assert_eq!(idx.len(), n, "[{scenario}] line {ln}: index size disagrees");
                for spec in f {
                    let mut p = spec.split(':');
                    let z: i32 = p.next().unwrap().parse().unwrap();
                    let want_distinct: usize = p.next().unwrap().parse().unwrap();
                    let want_hash: u64 = p.next().unwrap().parse().unwrap();

                    buf.clear();
                    let mut distinct = BTreeSet::new();
                    let mut first = true;
                    for &id in &live {
                        let rep = idx
                            .representative(id, z)
                            .unwrap_or_else(|| panic!("[{scenario}] device {id} vanished"));
                        if !first {
                            buf.push(',');
                        }
                        first = false;
                        buf.push_str(itoa(id).as_str());
                        buf.push('>');
                        buf.push_str(itoa(rep).as_str());
                        distinct.insert(rep);
                    }
                    assert_eq!(
                        distinct.len(),
                        want_distinct,
                        "[{scenario}] line {ln} z={z}: {} clusters, JavaScript produced {want_distinct}",
                        distinct.len()
                    );
                    assert_eq!(
                        fnv64(&buf),
                        want_hash,
                        "[{scenario}] line {ln} z={z}: same cluster COUNT ({want_distinct}) but a \
                         different grouping than JavaScript -- some device is in the wrong cluster"
                    );
                }
                checkpoints += 1;
            }
            "end" => {
                let idx = nc.as_ref().unwrap();
                idx.verify()
                    .unwrap_or_else(|e| panic!("[{scenario}] invariants broken at end: {e}"));
            }
            other => panic!("line {ln}: unknown record {other:?}"),
        }
    }

    assert!(scenarios >= 7, "fixture only had {scenarios} scenarios");
    assert!(
        checkpoints >= 70,
        "fixture only had {checkpoints} checkpoints"
    );
    assert!(ops > 15_000, "fixture only had {ops} operations");
    eprintln!(
        "  agreed with JavaScript across {scenarios} scenarios, {ops} operations, \
         {checkpoints} full-partition checkpoints"
    );
}

fn itoa(n: u64) -> String {
    n.to_string()
}
