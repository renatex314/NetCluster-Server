//! Snapshot and restore, at the collection level.
//!
//! The claim being tested is narrow and precise: a restored collection holds the
//! same devices, in the same places, in the same categories, with the same report
//! times. Not the same tree -- a restore inserts in snapshot order rather than the
//! original operation order, so cluster ids and groupings legitimately differ.

use netcluster_server::collection::{Collection, Config, Report};
use netcluster_server::snapshot;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ncpersist-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(categories: &[&str], ttl: u64) -> Config {
    Config {
        categories: categories.iter().map(|s| s.to_string()).collect(),
        ttl_seconds: ttl,
        ..Default::default()
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
}

/// Build a collection, churn it, snapshot it, restore it, and compare every device.
#[test]
fn a_restored_collection_holds_the_same_devices() {
    let dir = tmpdir("roundtrip");
    let cats = ["idle", "enroute", "delivering"];
    let c = Collection::new("fleet", cfg(&cats, 0));

    let mut rng = Rng(42);
    let mut expected: Vec<(String, u32)> = Vec::new();
    for i in 0..3000u32 {
        let id = format!("v{i}");
        let cat = i % 3;
        c.upsert(&[Report {
            id: &id,
            lng: -46.63 + (rng.next() - 0.5) * 0.9,
            lat: -23.55 + (rng.next() - 0.5) * 0.9,
            cat,
        }])
        .unwrap();
        expected.push((id, cat));
    }
    // churn: move some, remove some. Removed devices must not come back.
    for i in (0..3000u32).step_by(7) {
        let id = format!("v{i}");
        c.upsert(&[Report {
            id: &id,
            lng: 2.35,
            lat: 48.85,
            cat: i % 3,
        }])
        .unwrap();
    }
    for i in (0..3000u32).step_by(11) {
        c.remove(&format!("v{i}"));
    }
    expected.retain(|(id, _)| c.contains(id));
    assert!(expected.len() > 2500 && expected.len() < 3000);

    let before: Vec<_> = expected
        .iter()
        .map(|(id, _)| c.device(id).unwrap())
        .collect();

    let path = snapshot::path_for(&dir, "fleet");
    let bytes = c.snapshot_to(&path).unwrap();
    assert!(bytes > 0);

    let (meta, records) = snapshot::read(&path).unwrap();
    assert_eq!(meta.name, "fleet");
    assert_eq!(meta.config, cfg(&cats, 0), "config must survive");
    assert_eq!(records.len(), expected.len());

    let (restored, skipped) = Collection::restore(&meta.name, meta.config, &records);
    assert_eq!(skipped, 0, "ttl is 0, nothing should be skipped");
    assert_eq!(restored.len(), expected.len());

    for d in &before {
        let after = restored
            .device(&d.id)
            .unwrap_or_else(|| panic!("{} did not come back", d.id));
        assert_eq!(after.lng, d.lng, "{} moved", d.id);
        assert_eq!(after.lat, d.lat, "{} moved", d.id);
        assert_eq!(after.cat_index, d.cat_index, "{} changed category", d.id);
        assert_eq!(after.cat, d.cat);
        assert_eq!(
            after.last_seen_ms, d.last_seen_ms,
            "{} lost its report time",
            d.id
        );
    }
    for i in (0..3000u32).step_by(11) {
        assert!(
            !restored.contains(&format!("v{i}")),
            "a removed device came back from the snapshot"
        );
    }
    restored.verify().expect("the restored index must be sound");

    // and the restored collection is a working collection, not a frozen one
    restored
        .upsert(&[Report {
            id: "brand-new",
            lng: 1.0,
            lat: 1.0,
            cat: 0,
        }])
        .unwrap();
    assert!(restored.contains("brand-new"));
    restored.verify().unwrap();
}

/// Positions go through the snapshot as fixed-point, so a device that is never
/// touched must land on exactly the same coordinates however many restarts it sees.
#[test]
fn repeated_restores_do_not_drift() {
    let dir = tmpdir("drift");
    let mut c = Collection::new("fleet", cfg(&[], 0));
    c.upsert(&[Report {
        id: "still",
        lng: -46.633308,
        lat: -23.550520,
        cat: 0,
    }])
    .unwrap();
    let first = c.device("still").unwrap();

    for round in 0..10 {
        let path = snapshot::path_for(&dir, "fleet");
        c.snapshot_to(&path).unwrap();
        let (meta, records) = snapshot::read(&path).unwrap();
        c = Collection::restore(&meta.name, meta.config, &records).0;
        let d = c.device("still").unwrap();
        assert_eq!(d.lng, first.lng, "longitude drifted by round {round}");
        assert_eq!(d.lat, first.lat, "latitude drifted by round {round}");
    }
}

/// A snapshot from long enough ago restores nothing: those devices went quiet and
/// the sweep would delete them within seconds anyway.
#[test]
fn records_past_the_ttl_are_not_restored() {
    let now = netcluster_server::collection::now_ms();
    let records = vec![
        snapshot::DeviceRecord {
            id: "fresh".into(),
            x: 100,
            y: 100,
            cat: 0,
            last_seen_ms: now,
        },
        snapshot::DeviceRecord {
            id: "recent".into(),
            x: 200,
            y: 200,
            cat: 0,
            last_seen_ms: now - 30_000,
        },
        snapshot::DeviceRecord {
            id: "stale".into(),
            x: 300,
            y: 300,
            cat: 0,
            last_seen_ms: now - 600_000,
        },
        snapshot::DeviceRecord {
            id: "ancient".into(),
            x: 400,
            y: 400,
            cat: 0,
            last_seen_ms: 0,
        },
    ];
    let (c, skipped) = Collection::restore("fleet", cfg(&[], 60), &records);
    assert_eq!(skipped, 2, "stale and ancient should have been dropped");
    assert!(c.contains("fresh") && c.contains("recent"));
    assert!(!c.contains("stale") && !c.contains("ancient"));

    // ttl 0 disables expiry, so everything comes back
    let (c2, skipped2) = Collection::restore("fleet", cfg(&[], 0), &records);
    assert_eq!(skipped2, 0);
    assert_eq!(c2.len(), 4);
}

/// The config can have changed since the snapshot was written. A category that no
/// longer exists must not panic on the startup path, where a panic means the
/// process never comes back at all.
#[test]
fn a_shrunken_category_list_does_not_take_the_process_down() {
    let now = netcluster_server::collection::now_ms();
    let records = vec![
        snapshot::DeviceRecord {
            id: "a".into(),
            x: 100,
            y: 100,
            cat: 0,
            last_seen_ms: now,
        },
        snapshot::DeviceRecord {
            id: "b".into(),
            x: 200,
            y: 200,
            cat: 5,
            last_seen_ms: now,
        },
    ];
    let (c, _) = Collection::restore("fleet", cfg(&["only-one"], 0), &records);
    assert_eq!(c.len(), 2, "both devices should still be here");
    assert_eq!(
        c.device("b").unwrap().cat_index,
        0,
        "the missing category falls back to 0"
    );
    c.verify().unwrap();

    // and with categories removed entirely
    let (c2, _) = Collection::restore("fleet", cfg(&[], 0), &records);
    assert_eq!(c2.len(), 2);
    c2.verify().unwrap();
}

#[test]
fn an_empty_collection_round_trips_so_its_config_survives() {
    let dir = tmpdir("emptycfg");
    let cats = ["idle", "enroute"];
    let c = Collection::new("fleet", cfg(&cats, 900));
    let path = snapshot::path_for(&dir, "fleet");
    c.snapshot_to(&path).unwrap();
    let (meta, records) = snapshot::read(&path).unwrap();
    assert!(records.is_empty());
    // This is what closes the auto-create hazard: a restart brings the geometry
    // back instead of silently recreating a default collection.
    assert_eq!(
        meta.config.categories,
        vec!["idle".to_string(), "enroute".to_string()]
    );
    assert_eq!(meta.config.ttl_seconds, 900);
}

/// Snapshots taken while a writer is saturating ingest must still restore to a
/// sound index -- the export copies under a read lock, so it can never observe a
/// half-applied mutation.
#[test]
fn snapshots_during_heavy_writes_stay_consistent() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tmpdir("concurrent");
    let c = Arc::new(Collection::new("fleet", cfg(&["a", "b"], 0)));
    let mut rng = Rng(7);
    let ids: Vec<String> = (0..5000).map(|i| format!("v{i}")).collect();
    let initial: Vec<Report> = ids
        .iter()
        .map(|id| Report {
            id,
            lng: -46.63 + (rng.next() - 0.5) * 0.9,
            lat: -23.55 + (rng.next() - 0.5) * 0.9,
            cat: 0,
        })
        .collect();
    c.upsert(&initial).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (c, stop, ids) = (c.clone(), stop.clone(), ids.clone());
        std::thread::spawn(move || {
            let mut d = 0.0;
            while !stop.load(Ordering::Relaxed) {
                d += 0.00001;
                let batch: Vec<Report> = ids
                    .iter()
                    .take(1000)
                    .map(|id| Report {
                        id,
                        lng: -46.63 + d,
                        lat: -23.55,
                        cat: 1,
                    })
                    .collect();
                c.upsert(&batch).unwrap();
            }
        })
    };

    let path = snapshot::path_for(&dir, "fleet");
    for round in 0..12 {
        c.snapshot_to(&path).unwrap();
        let (meta, records) = snapshot::read(&path).unwrap();
        assert_eq!(records.len(), 5000, "round {round}: lost devices mid-write");
        let (restored, _) = Collection::restore(&meta.name, meta.config, &records);
        restored
            .verify()
            .unwrap_or_else(|e| panic!("round {round}: restored index is broken: {e}"));
        assert_eq!(restored.len(), 5000);
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    c.verify().unwrap();
}
