//! Filtering on several properties at once, over HTTP.
//!
//! Driven through the real router: the interesting parts are the query grammar,
//! which values reach the index from which body shape, and whether a filter that
//! cannot be answered fails loudly instead of returning an empty map.

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

fn dim(name: &str, values: &[&str], multi: bool) -> Dimension {
    Dimension {
        name: name.into(),
        values: values.iter().map(|s| s.to_string()).collect(),
        multi,
    }
}

/// client is multi-valued, status is not, and all three shapes are declared.
fn fleet_config() -> Config {
    Config {
        dimensions: vec![
            dim("client", &["1", "7", "22"], true),
            dim("status", &["idle", "enroute"], false),
        ],
        filters: vec![
            vec!["client".into()],
            vec!["status".into()],
            vec!["client".into(), "status".into()],
        ],
        ttl_seconds: 0,
        ..Default::default()
    }
}

fn state_with(cfg: Config) -> Arc<AppState> {
    let mut cs = HashMap::new();
    cs.insert("fleet".to_string(), Arc::new(Collection::new("fleet", cfg)));
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
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

/// Devices behind a query, counting cluster members rather than markers.
async fn count(s: &Arc<AppState>, query: &str) -> i64 {
    let (st, v) = call(
        s,
        "GET",
        &format!("/v1/collections/fleet/clusters?bbox=-180,-85,180,85&zoom=16&{query}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{query}: {v}");
    v["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["point_count"].as_i64().unwrap_or(1))
        .sum()
}

const WORLD: &str = "bbox=-180,-85,180,85&zoom=16";

#[tokio::test]
async fn a_conjunction_is_exact_and_a_device_may_hold_several_values() {
    let s = state_with(fleet_config());
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"dims":{"client":["1","7"],"status":"enroute"}},
            {"id":"b","lng":-46.64,"lat":-23.56,"dims":{"client":["7"],"status":"idle"}},
            {"id":"c","lng":-46.65,"lat":-23.57,"dims":{"client":["22"],"status":"enroute"}},
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["accepted"], 3);

    assert_eq!(count(&s, "f.client=7").await, 2, "a and b");
    assert_eq!(count(&s, "f.client=1").await, 1, "a only");
    assert_eq!(count(&s, "f.status=enroute").await, 2, "a and c");
    assert_eq!(
        count(&s, "f.client=7&f.status=enroute").await,
        1,
        "the conjunction is not the product of the marginals"
    );
    assert_eq!(count(&s, "f.client=7&f.status=idle").await, 1);
    assert_eq!(count(&s, "f.client=22&f.status=idle").await, 0);
    assert_eq!(count(&s, "").await, 3, "unfiltered");
}

#[tokio::test]
async fn geojson_carries_the_same_values_in_properties() {
    let s = state_with(fleet_config());
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({
            "type": "FeatureCollection",
            "features": [
                {"type":"Feature","id":"a","geometry":{"type":"Point","coordinates":[-46.63,-23.55]},
                 "properties":{"client":[1,7],"status":"enroute","plate":"ABC"}},
                {"type":"Feature","id":"b","geometry":{"type":"Point","coordinates":[-46.64,-23.56]},
                 "properties":{"client":[7],"status":"idle"}},
            ]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(count(&s, "f.client=7").await, 2);
    assert_eq!(count(&s, "f.client=7&f.status=enroute").await, 1);
    assert_eq!(count(&s, "f.client=1").await, 1);

    // properties are still stored verbatim alongside the filter values
    let (_, v) = call(&s, "GET", "/v1/collections/fleet/devices/a", None).await;
    assert_eq!(v["props"]["plate"], "ABC");
}

#[tokio::test]
async fn a_standing_device_changes_filters_when_its_values_change() {
    let s = state_with(fleet_config());
    let body = |status: &str| {
        json!({"points": [
            {"id":"parked","lng":-46.63,"lat":-23.55,"dims":{"client":["7"],"status":status}}
        ]})
    };
    call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(body("idle")),
    )
    .await;
    assert_eq!(count(&s, "f.status=idle").await, 1);

    // identical position, different status
    let (st, _) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(body("enroute")),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(count(&s, "f.status=idle").await, 0, "left the old status");
    assert_eq!(count(&s, "f.status=enroute").await, 1, "joined the new one");
    assert_eq!(count(&s, "f.client=7").await, 1, "client is untouched");
}

#[tokio::test]
async fn a_bare_position_report_leaves_the_values_alone() {
    // Positions arrive many times a second and carry no filter values; if that
    // re-filed the device into whatever sits at index 0, a fleet would collapse
    // into one bucket within a second of starting up.
    let s = state_with(fleet_config());
    call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"v","lng":-46.63,"lat":-23.55,"dims":{"client":["22"],"status":"enroute"}}
        ]})),
    )
    .await;
    let (st, _) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [{"id":"v","lng":-46.70,"lat":-23.60}]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(count(&s, "f.client=22").await, 1);
    assert_eq!(count(&s, "f.status=enroute").await, 1);
    assert_eq!(count(&s, "f.client=1").await, 0);
}

#[tokio::test]
async fn co_located_devices_are_all_reachable_by_filter() {
    // The failure this exists to fix: 20 vehicles parked in one depot come back
    // as a single cluster at every zoom, so a filter applied outside the index
    // sees one feature with no id and no properties and drops all twenty.
    let s = state_with(fleet_config());
    let mut pts = Vec::new();
    for i in 0..20 {
        pts.push(
            json!({"id": format!("depot-{i}"), "lng": -46.6333, "lat": -23.5505,
                        "dims": {"client": ["7"], "status": "idle"}}),
        );
    }
    for i in 0..30 {
        pts.push(
            json!({"id": format!("road-{i}"), "lng": -46.63 + (i as f64 - 15.0) * 0.02,
                        "lat": -23.55 + (i as f64 - 15.0) * 0.02,
                        "dims": {"client": ["7"], "status": "idle"}}),
        );
    }
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({ "points": pts })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        count(&s, "f.client=7").await,
        50,
        "all fifty, depot included"
    );

    // and the markers really are fewer than the devices, i.e. they did cluster
    let (_, v) = call(
        &s,
        "GET",
        &format!("/v1/collections/fleet/clusters?{WORLD}&f.client=7"),
        None,
    )
    .await;
    assert!(
        v["features"].as_array().unwrap().len() < 50,
        "the depot should still be drawn as one marker"
    );
}

#[tokio::test]
async fn a_filter_that_cannot_be_answered_fails_loudly() {
    let s = state_with(Config {
        dimensions: vec![
            dim("client", &["1", "7"], false),
            dim("status", &["idle"], false),
        ],
        // deliberately no ["client","status"] shape
        filters: vec![vec!["client".into()], vec!["status".into()]],
        ttl_seconds: 0,
        ..Default::default()
    });
    for (query, needle) in [
        ("f.client=1&f.status=idle", "no declared filter combines"),
        ("f.nope=1", "unknown filter"),
        ("f.client=999", "unknown value"),
    ] {
        let (st, v) = call(
            &s,
            "GET",
            &format!("/v1/collections/fleet/clusters?{WORLD}&{query}"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{query}: {v}");
        assert_eq!(v["code"], "bad_filter", "{query}: {v}");
        assert!(
            v["error"].as_str().unwrap().contains(needle),
            "{query}: {v}"
        );
    }
}

#[tokio::test]
async fn cat_and_f_are_not_mixed() {
    let s = state_with(fleet_config());
    let (st, v) = call(
        &s,
        "GET",
        &format!("/v1/collections/fleet/clusters?{WORLD}&cat=idle&f.client=7"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("not both"), "{v}");
}

#[tokio::test]
async fn an_undeclared_value_is_refused_at_ingest() {
    let s = state_with(fleet_config());
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"dims":{"status":"exploded"}}
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("unknown value"),
        "{v}"
    );
}

#[tokio::test]
async fn a_single_valued_dimension_refuses_a_list() {
    let s = state_with(fleet_config());
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"dims":{"status":["idle","enroute"]}}
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("not declared multi"),
        "{v}"
    );
}

#[tokio::test]
async fn the_categories_spelling_still_works() {
    // 0.3 collections keep their config, their ingest and their ?cat= query.
    let s = state_with(Config {
        categories: vec!["idle".into(), "enroute".into()],
        ttl_seconds: 0,
        ..Default::default()
    });
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"cat":"enroute"},
            {"id":"b","lng":-46.64,"lat":-23.56,"cat":0},
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(count(&s, "cat=enroute").await, 1);
    assert_eq!(count(&s, "cat=idle").await, 1);
    assert_eq!(count(&s, "cat=1").await, 1, "by index too");
    assert_eq!(count(&s, "").await, 2);

    // and GeoJSON still finds it under `cat` or `category`
    call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"type":"FeatureCollection","features":[
            {"type":"Feature","id":"c","geometry":{"type":"Point","coordinates":[-46.66,-23.58]},
             "properties":{"category":"enroute"}}
        ]})),
    )
    .await;
    assert_eq!(count(&s, "cat=enroute").await, 2);
}

#[tokio::test]
async fn a_collection_can_be_created_with_dimensions_over_http() {
    let s = state_with(Config::default());
    let (st, v) = call(
        &s,
        "PUT",
        "/v1/collections/owned",
        Some(json!({
            "dimensions": [
                {"name": "client", "values": ["1", "7"], "multi": true},
                {"name": "status", "values": ["idle", "enroute"]}
            ],
            "filters": [["client"], ["status"], ["client", "status"]],
            "ttl_seconds": 0
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/owned/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"dims":{"client":["1","7"],"status":"enroute"}}
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (st, v) = call(
        &s,
        "GET",
        &format!("/v1/collections/owned/clusters?{WORLD}&f.client=7&f.status=enroute"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["features"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn declaring_both_categories_and_dimensions_is_refused() {
    let s = state_with(Config::default());
    let (st, v) = call(
        &s,
        "PUT",
        "/v1/collections/bad",
        Some(json!({
            "categories": ["a"],
            "dimensions": [{"name": "client", "values": ["1"]}]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("not both"), "{v}");
}

#[tokio::test]
async fn tiles_take_the_same_filter() {
    let s = state_with(fleet_config());
    call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [
            {"id":"a","lng":-46.63,"lat":-23.55,"dims":{"client":["7"],"status":"enroute"}},
            {"id":"b","lng":-46.64,"lat":-23.56,"dims":{"client":["1"],"status":"idle"}},
        ]})),
    )
    .await;
    let (st, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/tiles/10/379/580.json?f.client=7",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let n: i64 = v["features"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| f["properties"]["point_count"].as_i64().unwrap_or(1))
                .sum()
        })
        .unwrap_or(0);
    assert_eq!(n, 1, "only the client-7 vehicle: {v}");
}
