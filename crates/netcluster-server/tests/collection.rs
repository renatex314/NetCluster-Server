//! What the server layer adds on top of the index: id interning, category
//! labels, expiry, and concurrent reads while a writer is running.

use netcluster_server::collection::{Collection, Config, Report};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

fn cfg(categories: &[&str], ttl: u64) -> Config {
    Config {
        categories: categories.iter().map(|s| s.to_string()).collect(),
        ttl_seconds: ttl,
        ..Default::default()
    }
}

fn report<'a>(id: &'a str, lng: f64, lat: f64, cat: u32) -> Report<'a> {
    Report { id, lng, lat, cat }
}

#[test]
fn string_ids_survive_the_round_trip() {
    let c = Collection::new("t", cfg(&[], 0));
    c.upsert(&[
        report("truck-1", -46.6333, -23.5505, 0),
        report("ônibus-2", -46.6340, -23.5510, 0),
        report("", -43.1729, -22.9068, 0),
    ])
    .unwrap();
    assert_eq!(c.len(), 3);
    let fs = c.clusters([-180.0, -85.0, 180.0, 85.0], 16.0, -1);
    let mut names: Vec<String> = fs.into_iter().filter_map(|f| f.device).collect();
    names.sort();
    assert_eq!(names, vec!["", "truck-1", "ônibus-2"]);
}

#[test]
fn reinterning_a_removed_id_reuses_its_slot() {
    let c = Collection::new("t", cfg(&[], 0));
    c.upsert(&[report("a", 0.0, 0.0, 0)]).unwrap();
    assert!(c.remove("a"));
    assert_eq!(c.len(), 0);
    assert!(!c.remove("a"), "removing twice must report false");
    c.upsert(&[report("a", 1.0, 1.0, 0)]).unwrap();
    assert_eq!(c.len(), 1);
    let f = &c.clusters([-10.0, -10.0, 10.0, 10.0], 16.0, -1)[0];
    assert_eq!(f.device.as_deref(), Some("a"));
    assert!(
        (f.lng - 1.0).abs() < 1e-6,
        "reinsert kept the stale position"
    );
}

#[test]
fn category_labels_resolve_by_name_and_by_index() {
    let c = Collection::new("t", cfg(&["idle", "enroute", "delivering"], 0));
    assert_eq!(c.category(None).unwrap(), -1);
    assert_eq!(c.category(Some("")).unwrap(), -1);
    assert_eq!(c.category(Some("idle")).unwrap(), 0);
    assert_eq!(c.category(Some("delivering")).unwrap(), 2);
    assert_eq!(c.category(Some("2")).unwrap(), 2);
    // A typo must fail loudly. Returning "no matches" would look like a working
    // filter over an empty result, which is the worst possible failure here.
    assert!(c.category(Some("delivring")).is_err());
    assert!(c.category(Some("9")).is_err());
    let e = c.category(Some("nope")).unwrap_err();
    assert!(
        e.contains("idle"),
        "the error should list what is valid: {e}"
    );
}

#[test]
fn a_category_out_of_range_is_rejected_at_ingest() {
    let c = Collection::new("t", cfg(&["a", "b"], 0));
    let e = c.upsert(&[report("x", 0.0, 0.0, 5)]).unwrap_err();
    assert!(e.contains("category"), "{e}");
    assert_eq!(c.len(), 0, "a rejected batch must not be partially applied");
}

#[test]
fn non_finite_coordinates_are_rejected_before_anything_is_written() {
    let c = Collection::new("t", cfg(&[], 0));
    c.upsert(&[report("good", 1.0, 1.0, 0)]).unwrap();
    let e = c
        .upsert(&[report("ok", 2.0, 2.0, 0), report("bad", f64::NAN, 0.0, 0)])
        .unwrap_err();
    assert!(e.contains("non-finite"), "{e}");
    assert_eq!(
        c.len(),
        1,
        "the whole batch must be rejected, not half of it"
    );
}

#[test]
fn filtered_queries_go_through_the_label() {
    let c = Collection::new("t", cfg(&["idle", "enroute", "delivering"], 0));
    c.upsert(&[
        report("t1", -46.6333, -23.5505, 2),
        report("t2", -46.6340, -23.5510, 2),
        report("t3", -46.6350, -23.5520, 0),
        report("t4", -43.1729, -22.9068, 1),
    ])
    .unwrap();
    let bbox = [-60.0, -35.0, -30.0, -10.0];
    let cat = c.category(Some("delivering")).unwrap();
    let fs = c.clusters(bbox, 4.0, cat);
    let total: u32 = fs.iter().map(|f| f.count).sum();
    assert_eq!(total, 2, "only the two delivering trucks should appear");
    assert_eq!(
        c.clusters(bbox, 4.0, -1)
            .iter()
            .map(|f| f.count)
            .sum::<u32>(),
        4
    );
}

/// A device that stops reporting has to disappear, or clusters quietly fill with
/// ghosts and every count on the map drifts upward.
#[test]
fn expiry_drops_only_the_silent_devices() {
    let c = Collection::new("t", cfg(&[], 1));
    c.upsert(&[report("old", 0.0, 0.0, 0), report("older", 1.0, 1.0, 0)])
        .unwrap();
    assert_eq!(c.sweep(), 0, "nothing is stale yet");
    thread::sleep(std::time::Duration::from_millis(1100));
    // one device keeps reporting; the other has gone quiet
    c.upsert(&[report("old", 0.0, 0.0, 0)]).unwrap();
    assert_eq!(c.sweep(), 1, "exactly the silent device should be dropped");
    assert_eq!(c.len(), 1);
    let names: Vec<String> = c
        .clusters([-10.0, -10.0, 10.0, 10.0], 16.0, -1)
        .into_iter()
        .filter_map(|f| f.device)
        .collect();
    assert_eq!(names, vec!["old"]);
    c.verify().unwrap();
}

#[test]
fn ttl_zero_disables_expiry() {
    let c = Collection::new("t", cfg(&[], 0));
    c.upsert(&[report("a", 0.0, 0.0, 0)]).unwrap();
    thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(c.sweep(), 0);
    assert_eq!(c.len(), 1);
}

/// The reason this is in Rust rather than in Lua inside Redis: readers run
/// *while* the writer runs. In the Redis version a wide query blocked every other
/// client for the whole query.
#[test]
fn readers_run_concurrently_with_a_writer() {
    let c = Arc::new(Collection::new("t", cfg(&[], 0)));
    let mut seed = 12345u64;
    let mut rnd = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 33) as f64 / (1u64 << 31) as f64
    };
    let batch: Vec<(String, f64, f64)> = (0..20_000)
        .map(|i| {
            (
                format!("v{i}"),
                -46.63 + (rnd() - 0.5) * 0.9,
                -23.55 + (rnd() - 0.5) * 0.9,
            )
        })
        .collect();
    c.upsert(
        &batch
            .iter()
            .map(|(id, lng, lat)| report(id, *lng, *lat, 0))
            .collect::<Vec<_>>(),
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));

    let writer = {
        let c = c.clone();
        let stop = stop.clone();
        let batch = batch.clone();
        thread::spawn(move || {
            let mut n = 0u64;
            let mut d = 0.0;
            while !stop.load(Ordering::Relaxed) {
                d += 0.00001;
                let rs: Vec<Report> = batch
                    .iter()
                    .take(2000)
                    .map(|(id, lng, lat)| report(id, lng + d, *lat, 0))
                    .collect();
                c.upsert(&rs).unwrap();
                n += rs.len() as u64;
            }
            n
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let c = c.clone();
            let stop = stop.clone();
            let reads = reads.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let fs = c.clusters([-48.0, -25.0, -45.0, -22.0], 9.0, -1);
                    assert!(!fs.is_empty(), "a query returned nothing mid-write");
                    // every marker must carry at least one device, always
                    assert!(fs.iter().all(|f| f.count >= 1));
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    thread::sleep(std::time::Duration::from_millis(600));
    stop.store(true, Ordering::Relaxed);
    let written = writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    let got = reads.load(Ordering::Relaxed);
    assert!(got > 50, "only {got} reads completed alongside the writer");
    assert!(written > 10_000, "only {written} positions written");
    c.verify().unwrap();
    eprintln!("  {got} concurrent queries alongside {written} position writes");
}

#[test]
fn tiles_carry_the_same_devices_as_the_bbox_query() {
    let c = Collection::new("t", cfg(&[], 0));
    let pts: Vec<(String, f64, f64)> = (0..500)
        .map(|i| {
            let f = i as f64;
            (
                format!("v{i}"),
                -46.63 + (f * 0.37).sin() * 0.4,
                -23.55 + (f * 0.71).cos() * 0.4,
            )
        })
        .collect();
    c.upsert(
        &pts.iter()
            .map(|(id, lng, lat)| report(id, *lng, *lat, 0))
            .collect::<Vec<_>>(),
    )
    .unwrap();

    for z in 0..4 {
        let side = 1i64 << z;
        let mut total = 0u32;
        for x in 0..side {
            for y in 0..side {
                for f in c.tile(z, x, y, -1) {
                    if f.x >= 0 && f.x < 512 && f.y >= 0 && f.y < 512 {
                        total += f.count;
                    }
                }
            }
        }
        assert_eq!(total, 500, "z={z}: tiles carried {total} of 500 devices");
    }
}

/// Registration must track the index, not the id-interning table. Interning is
/// permanent -- a removed device keeps its number so it lands back in the same
/// slot if it returns -- so asking the intern table "have I seen this id" would
/// answer yes forever.
#[test]
fn contains_reflects_the_index_not_the_intern_table() {
    let c = Collection::new("t", cfg(&["idle", "enroute"], 0));
    assert!(!c.contains("truck-1"), "empty collection");
    assert!(c.device("truck-1").is_none());

    c.upsert(&[report("truck-1", -46.6333, -23.5505, 1)])
        .unwrap();
    assert!(c.contains("truck-1"));
    assert!(!c.contains("truck-2"), "an id never reported");

    // this is the case the intern table would get wrong
    assert!(c.remove("truck-1"));
    assert!(!c.contains("truck-1"), "a removed device is not registered");
    assert!(c.device("truck-1").is_none());

    c.upsert(&[report("truck-1", 1.0, 1.0, 0)]).unwrap();
    assert!(c.contains("truck-1"), "reporting again re-registers it");
}

#[test]
fn device_reports_position_category_and_staleness() {
    let c = Collection::new("t", cfg(&["idle", "enroute", "delivering"], 300));
    c.upsert(&[report("truck-1", -46.6333, -23.5505, 2)])
        .unwrap();

    let d = c.device("truck-1").expect("registered");
    assert_eq!(d.id, "truck-1");
    assert!((d.lng - -46.6333).abs() < 1e-6, "lng {}", d.lng);
    assert!((d.lat - -23.5505).abs() < 1e-6, "lat {}", d.lat);
    assert_eq!(d.cat.as_deref(), Some("delivering"));
    assert_eq!(d.cat_index, 2);
    assert!(d.last_seen_ms > 0);
    assert!(
        d.age_ms < 5_000,
        "a device reported just now is {} ms stale",
        d.age_ms
    );

    // a move updates the position and refreshes the age, without losing the category
    thread::sleep(std::time::Duration::from_millis(30));
    let before = c.device("truck-1").unwrap().age_ms;
    assert!(before >= 25, "age did not advance: {before} ms");
    c.upsert(&[report("truck-1", -46.70, -23.60, 2)]).unwrap();
    let after = c.device("truck-1").unwrap();
    assert!(after.age_ms < before, "reporting did not refresh last_seen");
    assert!((after.lng - -46.70).abs() < 1e-6);
    assert_eq!(after.cat_index, 2);
}

#[test]
fn device_without_categories_still_answers() {
    let c = Collection::new("t", cfg(&[], 0));
    c.upsert(&[report("a", 1.0, 2.0, 0)]).unwrap();
    let d = c.device("a").unwrap();
    assert_eq!(d.cat, None, "no labels configured, so no label to report");
    assert_eq!(d.cat_index, 0);
}

/// An expired device must stop being registered, not linger as a ghost that
/// `contains` still vouches for.
#[test]
fn an_expired_device_is_no_longer_registered() {
    let c = Collection::new("t", cfg(&[], 1));
    c.upsert(&[report("ghost", 1.0, 1.0, 0)]).unwrap();
    assert!(c.contains("ghost"));
    thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(c.sweep(), 1);
    assert!(!c.contains("ghost"));
    assert!(c.device("ghost").is_none());
}
