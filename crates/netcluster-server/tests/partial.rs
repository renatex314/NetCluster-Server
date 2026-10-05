//! `?partial=true`: a batch is applied per device, and the reports the server
//! would not apply come back in `rejected` instead of failing the whole request.
//! Issue #10, point 1.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use netcluster_server::{
    collection::{Collection, Config},
    routes::{router_with_limit, AppState},
    schema::Dimension,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, RwLock},
};
use tower::ServiceExt;

fn app(config: Config) -> (Router, Arc<Collection>) {
    let c = Arc::new(Collection::new("fleet", config));
    let state = Arc::new(AppState {
        collections: RwLock::new(HashMap::from([("fleet".into(), c.clone())])),
        started_ms: 0,
        auto_create: false,
        requests: AtomicU64::new(0),
        data_dir: None,
    });
    (router_with_limit(state, 64), c)
}

async fn post(app: &Router, query: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/fleet/positions{query}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

const PARTIAL: &str = "?partial=true";

fn declared(name: &str, values: &[&str]) -> Dimension {
    Dimension {
        name: name.into(),
        capacity: None,
        values: values.iter().map(|v| v.to_string()).collect(),
        multi: false,
    }
}

fn capacity(name: &str, n: usize) -> Dimension {
    Dimension {
        name: name.into(),
        capacity: Some(n),
        values: vec![],
        multi: false,
    }
}

/// `(index, id, code)` of every rejected report, in batch order.
fn rejected(ack: &Value) -> Vec<(u64, Value, String)> {
    ack["rejected"]
        .as_array()
        .unwrap_or_else(|| panic!("no `rejected` in {ack}"))
        .iter()
        .map(|r| {
            assert!(r["error"].as_str().is_some_and(|e| !e.is_empty()), "{r}");
            (
                r["index"].as_u64().unwrap(),
                r["id"].clone(),
                r["code"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Every report sent comes back as exactly one of these -- the same check the
/// Node client makes, with `rejected` added.
fn accounted(ack: &Value) -> u64 {
    ack["accepted"].as_u64().unwrap()
        + ack["stale"].as_u64().unwrap()
        + ack["patched"].as_u64().unwrap_or(0)
        + ack["unknown"].as_array().map_or(0, |u| u.len() as u64)
        + ack["rejected"].as_array().map_or(0, |r| r.len() as u64)
}

fn compact_batch() -> Value {
    json!([
        {"id":"good-1","lng":1,"lat":1,"dims":{"status":"idle"}},
        {"id":"half","lng":2},
        {"id":"typo","lng":3,"lat":3,"dims":{"status":"idel"}},
        {"id":"fat","lng":4,"lat":4,"props":{"note":"x".repeat(200)}},
        {"id":"good-2","lng":5,"lat":5,"dims":{"status":"enroute"}}
    ])
}

fn compact_config() -> Config {
    Config {
        dimensions: vec![declared("status", &["idle", "enroute"])],
        max_props_bytes: 64,
        ..Config::default()
    }
}

#[tokio::test]
async fn without_the_flag_one_bad_device_still_fails_the_whole_batch() {
    let (app, c) = app(compact_config());
    let (status, err) = post(&app, "", compact_batch()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    // the same first error, with the same code, that this request always got
    assert_eq!(err["code"], "half_position", "{err}");
    assert!(err.get("rejected").is_none(), "{err}");
    assert_eq!(c.len(), 0, "nothing applied");

    let (status, _) = post(&app, "?partial=false", compact_batch()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(c.len(), 0);
}

#[tokio::test]
async fn with_the_flag_the_good_devices_land_and_the_bad_ones_are_named() {
    let (app, c) = app(compact_config());
    let (status, ack) = post(&app, PARTIAL, compact_batch()).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 2, "{ack}");
    assert_eq!(ack["stale"], 0, "{ack}");
    assert_eq!(
        rejected(&ack),
        [
            (1, json!("half"), "half_position".to_string()),
            (2, json!("typo"), "bad_request".to_string()),
            (3, json!("fat"), "bad_request".to_string()),
        ]
    );
    assert_eq!(accounted(&ack), 5);
    assert_eq!(c.len(), 2);
    assert!(c.device("good-1").is_some() && c.device("good-2").is_some());
    for id in ["half", "typo", "fat"] {
        assert!(c.device(id).is_none(), "{id} must not land");
    }
    c.verify().unwrap();
}

#[tokio::test]
async fn every_report_is_accounted_for_alongside_stale_patched_and_unknown() {
    let (app, _) = app(compact_config());
    post(
        &app,
        "",
        json!([{"id":"v","lng":1,"lat":1,"updated_at_ms":100}]),
    )
    .await;
    let (status, ack) = post(
        &app,
        PARTIAL,
        json!([
            {"id":"v","lng":1,"lat":1,"updated_at_ms":50},
            {"id":"v","dims":{"status":"enroute"},"updated_at_ms":200},
            {"id":"ghost","dims":{"status":"idle"}},
            {"id":"w","lng":2,"lat":2},
            {"id":"bad","dims":{"status":"nope"}}
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1, "{ack}");
    assert_eq!(ack["stale"], 1, "{ack}");
    assert_eq!(ack["patched"], 1, "{ack}");
    assert_eq!(ack["unknown"], json!(["ghost"]), "{ack}");
    assert_eq!(rejected(&ack).len(), 1, "{ack}");
    assert_eq!(accounted(&ack), 5, "{ack}");
}

#[tokio::test]
async fn a_full_capacity_rejects_only_the_device_that_would_overflow_it() {
    let (app, c) = app(Config {
        dimensions: vec![capacity("client", 1)],
        ..Config::default()
    });
    let (status, ack) = post(
        &app,
        PARTIAL,
        json!([
            {"id":"a","lng":1,"lat":1,"dims":{"client":"X"}},
            {"id":"b","lng":2,"lat":2,"dims":{"client":"Y"}},
            {"id":"c","lng":3,"lat":3,"dims":{"client":"X"}}
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 2, "{ack}");
    assert_eq!(rejected(&ack), [(1, json!("b"), "bad_request".to_string())]);
    assert_eq!(c.interned(), vec![1]);
    c.verify().unwrap();
}

/// Values are resolved after every other check, so a report refused for its
/// `props` never takes a slot it would then keep forever.
#[tokio::test]
async fn a_refused_device_takes_no_capacity_slot() {
    let (app, c) = app(Config {
        dimensions: vec![capacity("client", 4)],
        max_props_bytes: 16,
        ..Config::default()
    });
    let fat = json!([{
        "id":"fat","lng":1,"lat":1,
        "dims":{"client":"NEW"},
        "props":{"note":"far more than sixteen bytes"}
    }]);
    let (status, ack) = post(&app, PARTIAL, fat.clone()).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(rejected(&ack).len(), 1);
    let (status, _) = post(&app, "", fat).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(c.interned(), vec![0], "no slot spent on a refused report");
}

#[tokio::test]
async fn geojson_features_are_rejected_one_by_one() {
    let (app, c) = app(Config {
        dimensions: vec![declared("status", &["idle", "enroute"])],
        ..Config::default()
    });
    let feature = |id: Value, geom: Value, props: Value| {
        let mut f = json!({"type":"Feature","geometry":geom,"properties":props});
        if !id.is_null() {
            f["id"] = id;
        }
        f
    };
    let point = |lng: f64, lat: f64| json!({"type":"Point","coordinates":[lng, lat]});
    let body = json!({"type":"FeatureCollection","features":[
        feature(json!("good-1"), point(1., 1.), json!({"status":"idle"})),
        feature(json!("nowhere"), Value::Null, json!({})),
        feature(Value::Null, point(2., 2.), json!({"status":"idle"})),
        feature(json!("swapped"), point(-23.5, -146.6), json!({})),
        feature(json!("fraction"), point(3., 3.), json!({"status":1.5})),
        feature(json!("typo"), point(4., 4.), json!({"status":"idel"})),
        feature(json!("good-2"), point(5., 5.), json!({"status":"enroute"}))
    ]});

    let (status, err) = post(&app, "", body.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(c.len(), 0);

    let (status, ack) = post(&app, PARTIAL, body).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 2, "{ack}");
    let bad = rejected(&ack);
    let indices: Vec<u64> = bad.iter().map(|r| r.0).collect();
    assert_eq!(indices, [1, 2, 3, 4, 5], "{ack}");
    assert_eq!(
        bad[1].1,
        Value::Null,
        "a Feature with no id has none to name"
    );
    assert_eq!(bad[0].2, "bad_geojson");
    assert_eq!(accounted(&ack), 7);
    assert_eq!(c.len(), 2);
    c.verify().unwrap();
}

#[tokio::test]
async fn a_batch_with_nothing_good_is_still_an_answer() {
    let (app, c) = app(compact_config());
    let (status, ack) = post(&app, PARTIAL, json!([{"id":"half","lat":1}])).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 0);
    assert_eq!(rejected(&ack).len(), 1);
    assert_eq!(c.len(), 0);
}

#[tokio::test]
async fn the_flag_is_strict_and_always_answered() {
    let (app, _) = app(Config::default());
    let (status, err) = post(&app, "?partial=yes", json!([])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    // Present even when empty: it is how a client knows the flag was honoured.
    let (_, ack) = post(&app, PARTIAL, json!([])).await;
    assert_eq!(ack["rejected"], json!([]), "{ack}");
    let (_, ack) = post(&app, PARTIAL, json!([{"id":"v","lng":1,"lat":1}])).await;
    assert_eq!(ack["rejected"], json!([]), "{ack}");
    let (_, ack) = post(&app, "", json!([{"id":"v","lng":1,"lat":1}])).await;
    assert!(ack.get("rejected").is_none(), "unchanged without it: {ack}");
}
