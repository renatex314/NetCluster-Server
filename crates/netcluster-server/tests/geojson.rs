//! GeoJSON on the way in.
//!
//! Driven through the real router, because most of what can go wrong here is in
//! the extractors and the status codes rather than in the index: the format is
//! chosen by the shape of the body, the id may come from three places, and a
//! rejection is only useful if it names the feature it is about.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use netcluster_server::collection::{Collection, Config};
use netcluster_server::routes::{router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

fn state(categories: &[&str]) -> Arc<AppState> {
    let cfg = Config {
        categories: categories.iter().map(|s| s.to_string()).collect(),
        ttl_seconds: 0,
        ..Default::default()
    };
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
    let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn post(s: &Arc<AppState>, uri: &str, body: Value) -> (StatusCode, Value) {
    call(s, "POST", uri, Some(body)).await
}

fn feature(id: Value, lng: f64, lat: f64, props: Value) -> Value {
    let mut f = json!({
        "type": "Feature",
        "properties": props,
        "geometry": { "type": "Point", "coordinates": [lng, lat] }
    });
    if !id.is_null() {
        f["id"] = id;
    }
    f
}

fn fc(features: Vec<Value>) -> Value {
    json!({ "type": "FeatureCollection", "features": features })
}

/// Devices in a collection, as id -> [lng, lat], read back through /clusters at
/// the finest zoom so nothing is aggregated.
async fn devices(s: &Arc<AppState>) -> HashMap<String, (f64, f64)> {
    let (st, v) = call(s, "GET", "/v1/collections/fleet/clusters?zoom=20", None).await;
    assert_eq!(st, StatusCode::OK);
    v["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| !f["properties"]["cluster"].as_bool().unwrap_or(false))
        .map(|f| {
            let c = f["geometry"]["coordinates"].as_array().unwrap();
            (
                f["id"].as_str().unwrap().to_string(),
                (c[0].as_f64().unwrap(), c[1].as_f64().unwrap()),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_feature_collection_ingests() {
    let s = state(&[]);
    let (st, v) = post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![
            feature(json!("v1"), -46.63, -23.55, json!({ "plate": "ABC" })),
            feature(json!("v2"), -46.64, -23.56, Value::Null),
        ]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["accepted"], 2);
    assert_eq!(v["devices"], 2);

    let d = devices(&s).await;
    assert_eq!(d.len(), 2);
    assert!((d["v1"].0 - -46.63).abs() < 1e-6, "{:?}", d["v1"]);
    assert!((d["v1"].1 - -23.55).abs() < 1e-6);
}

#[tokio::test]
async fn the_compact_forms_still_work() {
    let s = state(&[]);
    let (st, _) = post(
        &s,
        "/v1/collections/fleet/positions",
        json!([{ "id": "a", "lng": 1.0, "lat": 2.0 }]),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = post(
        &s,
        "/v1/collections/fleet/positions",
        json!({ "points": [{ "id": "b", "lng": 3.0, "lat": 4.0 }] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(devices(&s).await.len(), 2);
}

#[tokio::test]
async fn geojson_and_the_compact_form_build_the_same_index() {
    let mut geo = Vec::new();
    let mut compact = Vec::new();
    for i in 0..500 {
        let lng = -46.63 + (i as f64 % 25.0) * 0.004;
        let lat = -23.55 + (i as f64 / 25.0) * 0.004;
        geo.push(feature(json!(format!("v{i}")), lng, lat, Value::Null));
        compact.push(json!({ "id": format!("v{i}"), "lng": lng, "lat": lat }));
    }
    let a = state(&[]);
    let b = state(&[]);
    post(&a, "/v1/collections/fleet/positions", fc(geo)).await;
    post(&b, "/v1/collections/fleet/positions", json!(compact)).await;

    for z in [0, 4, 8, 12, 16] {
        let uri = format!("/v1/collections/fleet/clusters?zoom={z}");
        let (_, va) = call(&a, "GET", &uri, None).await;
        let (_, vb) = call(&b, "GET", &uri, None).await;
        assert_eq!(va, vb, "zoom {z}: GeoJSON and compact ingest disagree");
    }
}

#[tokio::test]
async fn the_id_comes_from_three_places() {
    let s = state(&[]);
    // on the feature, as GeoJSON says
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(json!("onfeature"), 1.0, 1.0, Value::Null)]),
    )
    .await;
    // a numeric id becomes its decimal form, so 7 and "7" are one device
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(json!(7), 2.0, 2.0, Value::Null)]),
    )
    .await;
    // from properties when the feature has none
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(
            Value::Null,
            3.0,
            3.0,
            json!({ "id": "inprops" }),
        )]),
    )
    .await;
    // named explicitly
    let (st, v) = post(
        &s,
        "/v1/collections/fleet/positions?id_property=plate",
        fc(vec![feature(
            json!("ignored"),
            4.0,
            4.0,
            json!({ "plate": "ABC-9" }),
        )]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let d = devices(&s).await;
    let mut ids: Vec<&String> = d.keys().collect();
    ids.sort();
    assert_eq!(ids, vec!["7", "ABC-9", "inprops", "onfeature"]);
}

#[tokio::test]
async fn a_named_id_property_does_not_fall_back() {
    // Having asked for properties.plate, quietly using feature.id where it is
    // missing would key half a fleet one way and half the other.
    let s = state(&[]);
    let (st, v) = post(
        &s,
        "/v1/collections/fleet/positions?id_property=plate",
        fc(vec![feature(
            json!("has-an-id"),
            1.0,
            1.0,
            json!({ "other": 1 }),
        )]),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(
        v["error"].as_str().unwrap().contains("properties.plate"),
        "{v}"
    );
    assert_eq!(v["code"], "bad_geojson");
}

#[tokio::test]
async fn categories_come_from_properties() {
    let s = state(&["idle", "enroute", "delivering"]);
    let (st, v) = post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![
            feature(json!("byindex"), 1.0, 1.0, json!({ "cat": 2 })),
            feature(json!("byname"), 2.0, 2.0, json!({ "category": "enroute" })),
            feature(json!("none"), 3.0, 3.0, Value::Null),
        ]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (_, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/clusters?zoom=20&cat=delivering",
        None,
    )
    .await;
    let ids: Vec<&str> = v["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["byindex"]);

    let (_, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/clusters?zoom=20&cat=enroute",
        None,
    )
    .await;
    let ids: Vec<&str> = v["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["byname"]);
}

#[tokio::test]
async fn cat_property_renames_the_category_field() {
    let s = state(&["idle", "enroute"]);
    let (st, v) = post(
        &s,
        "/v1/collections/fleet/positions?cat_property=status",
        fc(vec![feature(
            json!("v1"),
            1.0,
            1.0,
            json!({ "status": "enroute", "cat": "idle" }),
        )]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = call(
        &s,
        "GET",
        "/v1/collections/fleet/clusters?zoom=20&cat=enroute",
        None,
    )
    .await;
    assert_eq!(v["features"].as_array().unwrap().len(), 1, "{v}");
}

#[tokio::test]
async fn properties_are_stored_verbatim_and_null_leaves_them_alone() {
    let s = state(&[]);
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(
            json!("v1"),
            1.0,
            1.0,
            json!({ "plate": "ABC", "nested": { "a": [1, 2] } }),
        )]),
    )
    .await;
    let (_, v) = call(&s, "GET", "/v1/collections/fleet/devices/v1", None).await;
    assert_eq!(v["props"]["plate"], "ABC");
    assert_eq!(v["props"]["nested"]["a"][1], 2);

    // GeoJSON's "no properties" must mean "leave what is stored alone", the same
    // as omitting `props` in the compact form.
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(json!("v1"), 5.0, 5.0, Value::Null)]),
    )
    .await;
    let (_, v) = call(&s, "GET", "/v1/collections/fleet/devices/v1", None).await;
    assert_eq!(
        v["props"]["plate"], "ABC",
        "null properties wiped the stored ones"
    );

    // an explicit {} still clears
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(json!("v1"), 6.0, 6.0, json!({}))]),
    )
    .await;
    let (_, v) = call(&s, "GET", "/v1/collections/fleet/devices/v1", None).await;
    assert!(v["props"]["plate"].is_null(), "{v}");
}

#[tokio::test]
async fn foreign_members_and_altitude_are_ignored() {
    let s = state(&[]);
    let body = json!({
        "type": "FeatureCollection",
        "bbox": [-180.0, -90.0, 180.0, 90.0],
        "generator": "some exporter",
        "features": [{
            "type": "Feature",
            "id": "v1",
            "bbox": [0.0, 0.0, 1.0, 1.0],
            "title": "a foreign member, which RFC 7946 allows",
            "properties": { "plate": "ABC" },
            "geometry": { "coordinates": [10.0, 20.0, 3000.0], "type": "Point" }
        }]
    });
    let (st, v) = post(&s, "/v1/collections/fleet/positions", body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let d = devices(&s).await;
    assert!(
        (d["v1"].0 - 10.0).abs() < 1e-6 && (d["v1"].1 - 20.0).abs() < 1e-6,
        "{d:?}"
    );
}

#[tokio::test]
async fn rejections_name_the_feature_and_the_reason() {
    let s = state(&[]);
    // 400 is this handler judging the content; 422 is serde judging the shape.
    // Both are pinned because clients switch on them, and both must come back in
    // this API's `{error, code}` body rather than as plain text.
    let cases: Vec<(Value, StatusCode, &str, &str)> = vec![
        (
            fc(vec![
                feature(json!("ok"), 1.0, 1.0, Value::Null),
                json!({ "type": "Feature", "id": "bad", "properties": null, "geometry": null }),
            ]),
            StatusCode::BAD_REQUEST,
            "bad_geojson",
            "features[1]",
        ),
        (
            fc(vec![
                json!({ "type": "Feature", "id": "p", "properties": null,
                            "geometry": { "type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 1.0]]] } }),
            ]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable_body",
            "geometry is a Polygon",
        ),
        (
            fc(vec![feature(Value::Null, 1.0, 1.0, Value::Null)]),
            StatusCode::BAD_REQUEST,
            "bad_geojson",
            "has no id",
        ),
        (
            fc(vec![feature(json!("swapped"), 35.68, 139.69, Value::Null)]),
            StatusCode::BAD_REQUEST,
            "bad_geojson",
            "longitude, latitude",
        ),
        (
            json!({ "points": [], "features": [] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable_body",
            "both `points` and `features`",
        ),
        (
            json!({ "type": "Feature", "properties": null }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable_body",
            "FeatureCollection",
        ),
    ];
    for (body, want_status, want_code, needle) in cases {
        let (st, v) = post(&s, "/v1/collections/fleet/positions", body).await;
        assert_eq!(st, want_status, "{v}");
        assert_eq!(v["code"], want_code, "{v}");
        let msg = v["error"].as_str().unwrap_or("");
        assert!(
            msg.contains(needle),
            "message {msg:?} does not mention {needle:?}"
        );
    }
    // nothing from a rejected batch may land
    assert!(devices(&s).await.is_empty());
}

#[tokio::test]
async fn a_body_that_is_not_json_is_still_rejected_as_json() {
    // The one failure mode hardest to diagnose remotely used to answer in plain
    // text, so a client doing `JSON.parse(await res.text())` threw on the error
    // instead of reporting it.
    let s = state(&[]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections/fleet/positions")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let res = router(s.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).expect("the rejection body must be JSON");
    assert_eq!(v["code"], "malformed_json");
}

#[tokio::test]
async fn an_empty_feature_collection_is_accepted_and_does_nothing() {
    let s = state(&[]);
    let (st, v) = post(&s, "/v1/collections/fleet/positions", fc(vec![])).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["accepted"], 0);
}

#[tokio::test]
async fn geojson_moves_a_device_that_already_exists() {
    let s = state(&[]);
    post(
        &s,
        "/v1/collections/fleet/positions",
        json!([{ "id": "v1", "lng": 1.0, "lat": 1.0 }]),
    )
    .await;
    post(
        &s,
        "/v1/collections/fleet/positions",
        fc(vec![feature(json!("v1"), 20.0, 30.0, Value::Null)]),
    )
    .await;
    let d = devices(&s).await;
    assert_eq!(d.len(), 1, "the GeoJSON report inserted a second device");
    assert!((d["v1"].0 - 20.0).abs() < 1e-6, "{d:?}");
}
