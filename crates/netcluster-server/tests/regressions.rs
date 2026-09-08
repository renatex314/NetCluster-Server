//! Correctness and overload regressions driven through the actual HTTP router.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use netcluster_server::{
    collection::{Collection, Config, Report, TextPred},
    routes::{router_with_limit, AppState},
    schema::Dimension,
    snapshot,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, RwLock},
    time::Duration,
};
use tower::ServiceExt;

fn app(config: Config, limit: usize) -> (Router, Arc<Collection>) {
    let c = Arc::new(Collection::new("fleet", config));
    let state = Arc::new(AppState {
        collections: RwLock::new(HashMap::from([("fleet".into(), c.clone())])),
        started_ms: 0,
        auto_create: false,
        requests: AtomicU64::new(0),
        data_dir: None,
    });
    (router_with_limit(state, limit), c)
}

async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
const POSITIONS: &str = "/v1/collections/fleet/positions";
const WORLD: [f64; 4] = [-180., -85., 180., 85.];

#[tokio::test]
async fn late_equal_and_unversioned_reports_cannot_change_versioned_position_or_props() {
    let (app, c) = app(Config::default(), 64);
    let (status, ack) = call(
        &app,
        "POST",
        POSITIONS,
        json!([{"id":"v","lng":20,"lat":20,"updated_at_ms":200,"props":{"plate":"NEW"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["accepted"], 1);
    for version in [json!(100), json!(200), Value::Null] {
        let (_, ack) = call(
            &app,
            "POST",
            POSITIONS,
            json!([{"id":"v","lng":1,"lat":1,"updated_at_ms":version,"props":{"plate":"OLD"}}]),
        )
        .await;
        assert_eq!(ack["accepted"], 0);
        assert_eq!(ack["stale"], 1);
    }
    let device = c.device("v").unwrap();
    assert!((device.lng - 20.).abs() < 1e-6);
    assert_eq!(device.updated_at_ms, Some(200));
    assert!(device.props.unwrap().get().contains("NEW"));
    assert_eq!(c.len(), 1);
}

#[tokio::test]
async fn compact_and_geojson_share_the_same_source_version() {
    let (app, c) = app(Config::default(), 64);
    let geo = |version, lng| {
        json!({"type":"FeatureCollection","features":[{
            "type":"Feature","id":"v","updated_at_ms":version,
            "geometry":{"type":"Point","coordinates":[lng,10]},"properties":{"plate":"NEW"}
        }]})
    };
    let (_, ack) = call(&app, "POST", POSITIONS, geo(200, 20)).await;
    assert_eq!(ack["accepted"], 1);
    let (_, ack) = call(
        &app,
        "POST",
        POSITIONS,
        json!([{"id":"v","lng":1,"lat":1,"updated_at_ms":100}]),
    )
    .await;
    assert_eq!(ack["stale"], 1);
    let (_, ack) = call(&app, "POST", POSITIONS, geo(150, 1)).await;
    assert_eq!(ack["stale"], 1);
    assert!((c.device("v").unwrap().lng - 20.).abs() < 1e-6);
}

#[tokio::test]
async fn a_full_dimension_never_reassigns_a_pending_batch_or_resolved_query() {
    let cfg = Config {
        dimensions: vec![Dimension {
            name: "client".into(),
            capacity: Some(1),
            values: vec![],
            multi: false,
        }],
        ..Config::default()
    };
    let (app, c) = app(cfg, 64);
    let (status, _) = call(
        &app,
        "POST",
        POSITIONS,
        json!([
            {"id":"a","lng":1,"lat":1,"dims":{"client":"X"}},
            {"id":"b","lng":2,"lat":2,"dims":{"client":"Y"}}
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        c.len(),
        0,
        "a rejected batch must not partially apply positions"
    );
    call(
        &app,
        "POST",
        POSITIONS,
        json!([{"id":"a","lng":1,"lat":1,"dims":{"client":"X"}}]),
    )
    .await;
    let selection = HashMap::from([("client".into(), "X".into())]);
    let cell = c.filter_cell(&selection).unwrap().unwrap();
    c.remove("a");
    let (status, _) = call(
        &app,
        "POST",
        POSITIONS,
        json!([{"id":"b","lng":2,"lat":2,"dims":{"client":"Y"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(c.clusters(WORLD, 20., cell).is_empty());
    assert_eq!(c.filter_cell(&selection).unwrap(), Some(cell));
}

#[tokio::test]
async fn overload_rejects_before_parsing_and_health_stays_available() {
    let (app, c) = app(Config::default(), 1);
    let guard = c.acquire_write().await;
    let pending_app = app.clone();
    let mut pending = Box::pin(call(
        &pending_app,
        "POST",
        POSITIONS,
        json!([{"id":"v","lng":1,"lat":1}]),
    ));
    // Poll the first request until it is waiting behind the held collection gate.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut pending)
            .await
            .is_err()
    );
    let request = Request::builder()
        .method("POST")
        .uri(POSITIONS)
        .header("content-type", "application/json")
        .body(Body::from("not JSON"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    let (status, _) = tokio::time::timeout(
        Duration::from_millis(250),
        call(&app, "GET", "/healthz", Value::Null),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    drop(guard);
    assert_eq!(pending.await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_writer_queue_has_a_finite_wait_budget() {
    let (app, c) = app(Config::default(), 64);
    let _guard = c.acquire_write().await;
    let (status, ack) = tokio::time::timeout(
        Duration::from_secs(3),
        call(&app, "POST", POSITIONS, json!([{"id":"v","lng":1,"lat":1}])),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ack["code"], "overloaded");
    assert_eq!(c.len(), 0);
}

#[tokio::test]
async fn eof_is_malformed_json_and_missing_devices_are_404() {
    let (app, _) = app(Config::default(), 64);
    let request = Request::builder()
        .method("POST")
        .uri(POSITIONS)
        .header("content-type", "application/json")
        .body(Body::from("["))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    for path in [
        "/v1/collections/fleet/devices/missing",
        "/v1/collections/fleet/devices/missing/cluster",
    ] {
        let (status, value) = call(&app, "GET", path, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["code"], "device_not_registered");
    }
}

#[test]
fn snapshot_preserves_source_version_props_and_searchable_text() {
    let cfg = Config {
        text: vec!["plate".into()],
        ttl_seconds: 0,
        ..Config::default()
    };
    let c = Collection::new("fleet", cfg);
    let props = serde_json::value::RawValue::from_string(r#"{"plate":"NEW"}"#.into()).unwrap();
    let report = Report {
        id: "v",
        lng: 20.,
        lat: 20.,
        props: Some(&props),
        cells: None,
        updated_at_ms: Some(200),
    };
    c.upsert(std::slice::from_ref(&report)).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "nc-version-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = snapshot::path_for(&dir, "fleet");
    c.snapshot_to(&path).unwrap();
    let (meta, records) = snapshot::read(&path).unwrap();
    let (restored, skipped) = Collection::restore(&meta.name, meta.config, &meta.labels, &records);
    assert_eq!(skipped, 0);
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored
            .upsert(&[Report {
                lng: 1.,
                updated_at_ms: Some(100),
                ..report
            }])
            .unwrap(),
        0
    );
    let d = restored.device("v").unwrap();
    assert_eq!(d.updated_at_ms, Some(200));
    assert!((d.lng - 20.).abs() < 1e-6);
    let hits = restored.search(
        WORLD,
        20.,
        -1,
        &[TextPred {
            field: 0,
            contains: false,
            needle: "new".into(),
        }],
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].device.as_deref(), Some("v"));
    assert!(restored.verify().is_ok());
    std::fs::remove_dir_all(&dir).unwrap();
}
