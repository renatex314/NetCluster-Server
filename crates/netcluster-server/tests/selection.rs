//! The three selection primitives that are not the aggregate path: an arbitrary id
//! whitelist, the flat device listing, and metadata-only updates.
//!
//! Driven through the real router, because most of what can go wrong here is in the
//! query grammar and in what each shape is allowed to mean -- an empty `?ids=`
//! matching everything, a listing that quietly collapses a depot, a patch that
//! resurrects an expired vehicle.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use netcluster_server::collection::{Collection, Config};
use netcluster_server::routes::{router, AppState};
use netcluster_server::schema::Dimension;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

fn dim(name: &str, values: &[&str]) -> Dimension {
    Dimension {
        name: name.into(),
        values: values.iter().map(|s| s.to_string()).collect(),
        capacity: None,
        multi: false,
    }
}

/// A fleet with a boolean flag, a status, and a searchable plate.
fn cfg() -> Config {
    Config {
        dimensions: vec![
            dim("flagged", &["true", "false"]),
            dim("status", &["idle", "enroute"]),
        ],
        filters: vec![
            vec!["flagged".into()],
            vec!["status".into()],
            vec!["flagged".into(), "status".into()],
        ],
        text: vec!["plate".into()],
        ttl_seconds: 0,
        ..Default::default()
    }
}

fn state() -> Arc<AppState> {
    let mut cs = HashMap::new();
    cs.insert(
        "fleet".to_string(),
        Arc::new(Collection::new("fleet", cfg())),
    );
    Arc::new(AppState {
        collections: RwLock::new(cs),
        started_ms: 0,
        auto_create: false,
        requests: AtomicU64::new(0),
        data_dir: None,
    })
}

async fn call(
    s: &Arc<AppState>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router(s.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 32 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(s: &Arc<AppState>, uri: &str) -> Value {
    let (st, v) = call(s, "GET", uri, None).await;
    assert_eq!(st, StatusCode::OK, "{uri}: {v}");
    v
}

async fn post(s: &Arc<AppState>, body: Value) -> (StatusCode, Value) {
    call(s, "POST", "/v1/collections/fleet/positions", Some(body)).await
}

/// Two vehicles stacked in one yard and one on its own, so every test has both the
/// "collapses into a marker" case and the "stands alone" case.
async fn seeded() -> Arc<AppState> {
    let s = state();
    let (st, v) = post(
        &s,
        json!([
            {"id":"v1","lng":-46.63,"lat":-23.55,"dims":{"flagged":"false","status":"idle"},"props":{"plate":"ABC1111"}},
            {"id":"v2","lng":-46.70,"lat":-23.60,"dims":{"flagged":"false","status":"enroute"},"props":{"plate":"ABC2222"}},
            {"id":"v3","lng":-46.70,"lat":-23.60,"dims":{"flagged":"false","status":"idle"},"props":{"plate":"XYZ3333"}}
        ]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "seed: {v}");
    assert_eq!(v["accepted"], 3);
    s
}

fn ids_of(v: &Value) -> Vec<String> {
    v["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str().map(String::from))
        .collect()
}

fn compact_ids(v: &Value) -> Vec<String> {
    v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect()
}

const WORLD: &str = "bbox=-180,-85,180,85&zoom=16";

// ------------------------------------------------------- id whitelist (#2) --

#[tokio::test]
async fn an_id_whitelist_returns_exactly_those_devices() {
    let s = seeded().await;
    let v = get(
        &s,
        &format!("/v1/collections/fleet/clusters?{WORLD}&ids=v1"),
    )
    .await;
    assert_eq!(ids_of(&v), ["v1"]);
}

/// The reason this is a server-side primitive: whitelisted devices that sit on top
/// of each other still come back as one marker of 2, exactly as the unfiltered
/// query would draw them. A caller filtering its own copy of the fleet cannot
/// reproduce that -- it would either draw two markers or lose one.
#[tokio::test]
async fn whitelisted_devices_in_one_yard_stay_one_marker() {
    let s = seeded().await;
    let v = get(
        &s,
        &format!("/v1/collections/fleet/clusters?{WORLD}&ids=v2,v3"),
    )
    .await;
    let f = &v["features"].as_array().unwrap()[0];
    assert_eq!(v["features"].as_array().unwrap().len(), 1);
    assert_eq!(f["properties"]["point_count"], 2);
    // not a node of the tree, so there is nothing to expand
    assert_eq!(f["properties"]["expandable"], false);
    assert!(f["properties"]["cluster_id"].is_null());
}

/// An empty whitelist is an empty answer. The other reading -- "no ids named, so
/// show everything" -- is the one genuinely dangerous answer: the caller's external
/// set is legitimately empty sometimes, and a map of every vehicle is not that.
#[tokio::test]
async fn an_empty_whitelist_matches_nothing_rather_than_everything() {
    let s = seeded().await;
    let v = get(&s, &format!("/v1/collections/fleet/clusters?{WORLD}&ids=")).await;
    assert!(v["features"].as_array().unwrap().is_empty(), "{v}");
    let v = get(&s, "/v1/collections/fleet/devices?ids=").await;
    assert_eq!(v["total"], 0, "{v}");
}

/// Unlike an undeclared filter *value*, which is a 400. A whitelist comes from
/// somewhere else and a vehicle may expire between that read and this query.
#[tokio::test]
async fn an_unknown_id_in_a_whitelist_is_skipped_not_an_error() {
    let s = seeded().await;
    let v = get(
        &s,
        &format!("/v1/collections/fleet/clusters?{WORLD}&ids=v1,never-existed"),
    )
    .await;
    assert_eq!(ids_of(&v), ["v1"]);
}

#[tokio::test]
async fn a_whitelist_intersects_with_the_declared_filters_and_with_where() {
    let s = seeded().await;
    let v = get(
        &s,
        &format!("/v1/collections/fleet/clusters?{WORLD}&ids=v1,v2,v3&f.status=idle"),
    )
    .await;
    let mut got = ids_of(&v);
    got.sort();
    assert_eq!(got, ["v1", "v3"]);

    let v = get(
        &s,
        "/v1/collections/fleet/devices?ids=v1,v2,v3&where=plate~abc&format=compact",
    )
    .await;
    let mut got = compact_ids(&v);
    got.sort();
    assert_eq!(got, ["v1", "v2"]);
}

#[tokio::test]
async fn too_many_ids_is_refused_rather_than_silently_truncated() {
    let s = seeded().await;
    let many: Vec<String> = (0..1001).map(|i| format!("v{i}")).collect();
    let (st, v) = call(
        &s,
        "GET",
        &format!("/v1/collections/fleet/devices?ids={}", many.join(",")),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["code"], "too_many_ids");
}

/// A tile is cached by coordinate and a whitelist is per request, so serving one
/// unfiltered would show every vehicle. Refused, like `?where=`.
#[tokio::test]
async fn tiles_refuse_a_whitelist() {
    let s = seeded().await;
    let (st, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/tiles/10/300/500.mvt?ids=v1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "ids_not_supported");
}

// ---------------------------------------------------------- flat listing (#3) --

/// The whole point: the listing does not group. The two vehicles in one yard are
/// two rows here and one marker in `/clusters`.
#[tokio::test]
async fn the_listing_never_collapses_coincident_devices() {
    let s = seeded().await;
    let clustered = get(&s, &format!("/v1/collections/fleet/clusters?{WORLD}")).await;
    let markers = clustered["features"].as_array().unwrap().len();
    assert_eq!(markers, 2, "expected the yard to collapse: {clustered}");

    let listed = get(&s, "/v1/collections/fleet/devices?format=compact").await;
    let mut got = compact_ids(&listed);
    got.sort();
    assert_eq!(got, ["v1", "v2", "v3"]);
    assert_eq!(listed["total"], 3);
}

#[tokio::test]
async fn the_listing_takes_the_same_filters_as_the_clustered_query() {
    let s = seeded().await;
    let v = get(
        &s,
        "/v1/collections/fleet/devices?f.status=idle&format=compact",
    )
    .await;
    let mut got = compact_ids(&v);
    got.sort();
    assert_eq!(got, ["v1", "v3"]);

    let v = get(
        &s,
        "/v1/collections/fleet/devices?where=plate~xyz&format=compact",
    )
    .await;
    assert_eq!(compact_ids(&v), ["v3"]);
}

#[tokio::test]
async fn the_listing_pages_stably_and_reports_the_total() {
    let s = seeded().await;
    let mut seen = Vec::new();
    for offset in [0, 2] {
        let v = get(
            &s,
            &format!("/v1/collections/fleet/devices?limit=2&offset={offset}&format=compact"),
        )
        .await;
        assert_eq!(v["total"], 3, "the total is of matches, not of the page");
        assert_eq!(v["limit"], 2);
        assert_eq!(v["offset"], offset);
        seen.extend(compact_ids(&v));
    }
    seen.sort();
    assert_eq!(seen, ["v1", "v2", "v3"], "paging lost or repeated a device");
}

#[tokio::test]
async fn the_listing_can_leave_properties_out() {
    let s = seeded().await;
    let v = get(&s, "/v1/collections/fleet/devices?format=compact").await;
    assert!(v["devices"][0]["props"].is_object());
    let v = get(
        &s,
        "/v1/collections/fleet/devices?props=false&format=compact",
    )
    .await;
    assert!(v["devices"][0]["props"].is_null());
    // the id and the position are never optional -- they are the listing
    assert!(v["devices"][0]["id"].is_string());
    assert!(v["devices"][0]["lng"].is_number());
}

#[tokio::test]
async fn the_listing_defaults_to_geojson_and_respects_a_bbox() {
    let s = seeded().await;
    let v = get(&s, "/v1/collections/fleet/devices").await;
    assert_eq!(v["type"], "FeatureCollection");
    let mut got = ids_of(&v);
    got.sort();
    assert_eq!(got, ["v1", "v2", "v3"]);

    // a box around the yard only
    let v = get(
        &s,
        "/v1/collections/fleet/devices?bbox=-46.71,-23.61,-46.69,-23.59",
    )
    .await;
    let mut got = ids_of(&v);
    got.sort();
    assert_eq!(got, ["v2", "v3"], "bbox did not narrow the listing: {v}");
    assert_eq!(v["total"], 2, "the total counts matches in the box");
}

#[tokio::test]
async fn an_absurd_page_size_is_refused_with_the_limit_named() {
    let s = seeded().await;
    let (st, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/devices?limit=999999",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "limit_too_large");
}

// ------------------------------------------------ metadata-only updates (#5) --

#[tokio::test]
async fn a_report_without_a_position_updates_only_the_values() {
    let s = seeded().await;
    let before = get(&s, "/v1/collections/fleet/devices?ids=v1&format=compact").await;
    let (lng, lat) = (
        before["devices"][0]["lng"].as_f64().unwrap(),
        before["devices"][0]["lat"].as_f64().unwrap(),
    );

    let (st, v) = post(&s, json!([{"id":"v1","dims":{"flagged":"true"}}])).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["accepted"], 0, "a patch is not a position report");
    assert_eq!(v["patched"], 1);
    assert_eq!(v["unknown"].as_array().unwrap().len(), 0);

    // the filter now finds it...
    let v = get(
        &s,
        "/v1/collections/fleet/devices?f.flagged=true&format=compact",
    )
    .await;
    assert_eq!(compact_ids(&v), ["v1"]);
    // ...and it has not moved a micrometre
    let after = get(&s, "/v1/collections/fleet/devices?ids=v1&format=compact").await;
    assert_eq!(after["devices"][0]["lng"].as_f64().unwrap(), lng);
    assert_eq!(after["devices"][0]["lat"].as_f64().unwrap(), lat);
}

#[tokio::test]
async fn a_position_less_report_can_replace_properties_and_the_search_follows() {
    let s = seeded().await;
    let (st, v) = post(&s, json!([{"id":"v1","props":{"plate":"NEW0001"}}])).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["patched"], 1);

    let v = get(
        &s,
        "/v1/collections/fleet/devices?where=plate~new&format=compact",
    )
    .await;
    assert_eq!(
        compact_ids(&v),
        ["v1"],
        "the searchable field did not follow"
    );
    let v = get(
        &s,
        "/v1/collections/fleet/devices?where=plate~abc1&format=compact",
    )
    .await;
    assert!(
        v["devices"].as_array().unwrap().is_empty(),
        "the old plate still matches"
    );
}

/// Both or neither. One alone is always a bug, and either guess -- dropping the
/// half that was sent, or moving the device to a made-up coordinate -- is worse
/// than refusing.
#[tokio::test]
async fn half_a_coordinate_is_refused() {
    let s = seeded().await;
    for body in [
        json!([{"id":"v1","lng":-46.0}]),
        json!([{"id":"v1","lat":-23.0}]),
    ] {
        let (st, v) = post(&s, body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["code"], "half_position");
    }
}

/// A device that is not live is reported back, not resurrected: its position is
/// unknown, and inventing one to hang a flag on is how a ghost gets on the map.
#[tokio::test]
async fn patching_an_unknown_device_is_reported_not_fatal() {
    let s = seeded().await;
    let (st, v) = post(
        &s,
        json!([{"id":"ghost","dims":{"flagged":"true"}}, {"id":"v1","dims":{"flagged":"true"}}]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["patched"], 1, "the good half of the batch applied");
    assert_eq!(v["unknown"], json!(["ghost"]));
}

/// A batch may mix the two shapes; each item is whichever it looks like.
#[tokio::test]
async fn one_batch_may_carry_both_reports_and_patches() {
    let s = seeded().await;
    let (st, v) = post(
        &s,
        json!([
            {"id":"v1","lng":-46.50,"lat":-23.40},
            {"id":"v2","dims":{"flagged":"true"}},
            {"id":"v9","lng":-46.10,"lat":-23.10,"dims":{"flagged":"true"}}
        ]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["accepted"], 2, "two carried a position");
    assert_eq!(v["patched"], 1);
    assert_eq!(v["devices"], 4);
    let v = get(
        &s,
        "/v1/collections/fleet/devices?f.flagged=true&format=compact",
    )
    .await;
    let mut got = compact_ids(&v);
    got.sort();
    assert_eq!(got, ["v2", "v9"]);
}

/// An ordinary batch of position reports must answer exactly as it always has --
/// no new keys appear unless a patch was actually sent.
#[tokio::test]
async fn an_ordinary_batch_answers_exactly_as_before() {
    let s = seeded().await;
    let (st, v) = post(&s, json!([{"id":"v1","lng":-46.6,"lat":-23.5}])).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["accepted"], 1);
    assert_eq!(v["stale"], 0);
    assert!(
        v["patched"].is_null(),
        "patched leaked into a plain report: {v}"
    );
    assert!(
        v["unknown"].is_null(),
        "unknown leaked into a plain report: {v}"
    );
}

/// The stats counter, so a patch is distinguishable from a report in operations.
#[tokio::test]
async fn patches_are_counted_apart_from_reports() {
    let s = seeded().await;
    post(&s, json!([{"id":"v1","dims":{"flagged":"true"}}])).await;
    let v = get(&s, "/v1/collections/fleet").await;
    assert_eq!(v["patched"], 1, "{v}");
    assert_eq!(v["ingested"], 3, "the seed, and not the patch");
}

/// Version checks apply to a patch as they do to a report: a late retry must not
/// undo the newer value.
#[tokio::test]
async fn a_stale_patch_is_ignored() {
    let s = seeded().await;
    post(
        &s,
        json!([{"id":"v1","lng":-46.63,"lat":-23.55,"updated_at_ms":1000}]),
    )
    .await;
    let (_, v) = post(
        &s,
        json!([{"id":"v1","dims":{"flagged":"true"},"updated_at_ms":500}]),
    )
    .await;
    assert_eq!(v["patched"], 0, "an older patch was applied: {v}");
    let v = get(
        &s,
        "/v1/collections/fleet/devices?f.flagged=true&format=compact",
    )
    .await;
    assert!(v["devices"].as_array().unwrap().is_empty());

    let (_, v) = post(
        &s,
        json!([{"id":"v1","dims":{"flagged":"true"},"updated_at_ms":2000}]),
    )
    .await;
    assert_eq!(v["patched"], 1, "a newer patch was rejected: {v}");
}

// ------------------------------------------- the TTL rule, without the router --

/// The decision worth pinning: a metadata-only update is **not** a heartbeat.
///
/// The position stream is what proves a vehicle is still out there. If flipping an
/// external flag renewed the TTL, a fleet whose billing system keeps touching
/// records would never expire anything, and the map would fill with vehicles that
/// stopped reporting hours ago -- the exact failure `ttl_seconds` exists to prevent.
#[test]
fn a_patch_is_not_a_heartbeat() {
    use netcluster_server::collection::{Patch, Report};

    let c = Collection::new(
        "t",
        Config {
            dimensions: vec![dim("flagged", &["true", "false"])],
            filters: vec![vec!["flagged".into()]],
            ttl_seconds: 1,
            ..Default::default()
        },
    );
    let quiet = Report {
        id: "quiet",
        lng: 0.0,
        lat: 0.0,
        props: None,
        cells: Some(&[1]),
        updated_at_ms: None,
    };
    let talking = Report {
        id: "talking",
        lng: 1.0,
        lat: 1.0,
        props: None,
        cells: Some(&[1]),
        updated_at_ms: None,
    };
    c.upsert(&[quiet.clone(), talking.clone()]).unwrap();
    assert_eq!(c.sweep(), 0, "nothing is stale yet");

    std::thread::sleep(std::time::Duration::from_millis(1100));

    // the quiet one gets a flag change; the talking one reports its position
    let out = c
        .patch(&[Patch {
            id: "quiet",
            cells: Some(&[0]),
            props: None,
            updated_at_ms: None,
        }])
        .unwrap();
    assert_eq!(out.applied, 1, "the patch itself must still apply");
    c.upsert(&[talking]).unwrap();

    assert_eq!(
        c.sweep(),
        1,
        "the patched-but-silent device should still expire"
    );
    assert_eq!(c.len(), 1);
    c.verify().unwrap();

    // and once it is gone, a patch does not bring it back
    let out = c
        .patch(&[Patch {
            id: "quiet",
            cells: Some(&[1]),
            props: None,
            updated_at_ms: None,
        }])
        .unwrap();
    assert_eq!(out.applied, 0);
    assert_eq!(out.unknown, vec!["quiet".to_string()]);
    assert_eq!(c.len(), 1, "an expired device was resurrected by a patch");
}

/// Patched state is real state, so it has to survive a restart.
///
/// Worth pinning because a patch writes through a different path than a report: it
/// touches the per-device records without going near the coordinates, and the
/// snapshot is built from exactly those records. A patch that updated the tree but
/// not the records would look correct until the process restarted.
#[test]
fn a_patch_survives_a_snapshot_round_trip() {
    use netcluster_server::collection::{Candidates, Page, Patch, Report, TextPred};
    use netcluster_server::snapshot;

    let dir = std::env::temp_dir().join(format!(
        "ncsel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let c = Collection::new("fleet", cfg());
    let plate = serde_json::value::RawValue::from_string(r#"{"plate":"OLD0001"}"#.into()).unwrap();
    c.upsert(&[Report {
        id: "v1",
        lng: -46.63,
        lat: -23.55,
        props: Some(&plate),
        // flagged=false, status=idle
        cells: Some(&[1]),
        updated_at_ms: None,
    }])
    .unwrap();

    // flip the flag and correct the plate, with no position in sight
    let newplate =
        serde_json::value::RawValue::from_string(r#"{"plate":"NEW0002"}"#.into()).unwrap();
    let out = c
        .patch(&[Patch {
            id: "v1",
            cells: Some(&[0]),
            props: Some(&newplate),
            updated_at_ms: None,
        }])
        .unwrap();
    assert_eq!(out.applied, 1);

    let path = snapshot::path_for(&dir, "fleet");
    c.snapshot_to(&path).unwrap();
    let (meta, records) = snapshot::read(&path).unwrap();
    let (restored, skipped) =
        Collection::restore(&meta.name, meta.config, &[Vec::new(), Vec::new()], &records);
    assert_eq!(skipped, 0);
    assert_eq!(restored.len(), 1);

    // the patched cell came back...
    let (found, total) = restored.list_devices(
        [-180.0, -85.0, 180.0, 85.0],
        0,
        &[],
        Candidates::All,
        Page::default(),
    );
    assert_eq!(
        total, 1,
        "the patched filter cell did not survive the restore"
    );
    assert_eq!(found[0].device.as_deref(), Some("v1"));

    // ...and so did the patched properties, including the searchable field, which
    // is re-extracted on the way back in rather than stored
    let pred = vec![TextPred {
        field: 0,
        contains: true,
        needle: "new".into(),
    }];
    let (found, _) = restored.list_devices(
        [-180.0, -85.0, 180.0, 85.0],
        -1,
        &pred,
        Candidates::All,
        Page::default(),
    );
    assert_eq!(
        found.len(),
        1,
        "the patched plate is not searchable after a restore"
    );

    std::fs::remove_dir_all(&dir).ok();
}
