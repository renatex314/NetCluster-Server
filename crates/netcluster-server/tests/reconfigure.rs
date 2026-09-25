//! What a repeated `PUT /v1/collections/:name` is allowed to change.
//!
//! The rule has two halves, and the interesting failures are in the seam between
//! them. Anything the tree is built from is frozen, and asking for a different one
//! is a 409 -- the alternative, answering `created: false` and keeping the old
//! geometry, is how a deployment ends up believing it configured a collection it
//! did not. The two limits that shape nothing are adopted in place instead, so
//! raising a TTL does not cost you every device in the collection.
//!
//! Driven through the real router: the decision lives in the handler, and what
//! matters is the status and the body a deploy script actually sees.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use netcluster_server::collection::Collection;
use netcluster_server::routes::{router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

fn state() -> Arc<AppState> {
    Arc::new(AppState {
        collections: RwLock::new(HashMap::new()),
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

async fn put(s: &Arc<AppState>, name: &str, body: Value) -> (StatusCode, Value) {
    call(s, "PUT", &format!("/v1/collections/{name}"), Some(body)).await
}

async fn stats(s: &Arc<AppState>, name: &str) -> Value {
    let (st, v) = call(s, "GET", &format!("/v1/collections/{name}"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn collection(s: &Arc<AppState>, name: &str) -> Arc<Collection> {
    s.collections.read().unwrap().get(name).cloned().unwrap()
}

/// A config that declares one of everything, so any single field can be perturbed
/// on its own and the 409 can only have come from that field.
fn base() -> Value {
    json!({
        "max_zoom": 14,
        "radius": 50.0,
        "extent": 512.0,
        "hysteresis": 0.25,
        "dimensions": [{"name": "status", "values": ["idle", "enroute"]}],
        "filters": [["status"]],
        "text": ["plate"],
        "ttl_seconds": 0,
        "max_props_bytes": 512
    })
}

fn with(field: &str, value: Value) -> Value {
    let mut body = base();
    body[field] = value;
    body
}

// ------------------------------------------------------------------ frozen --

#[tokio::test]
async fn an_identical_put_changes_nothing_and_says_so() {
    let s = state();
    let (st, v) = put(&s, "fleet", base()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["created"], json!(true));

    let (st, v) = put(&s, "fleet", base()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(
        v["adopted"],
        json!([]),
        "nothing moved, so nothing to report"
    );
}

/// The report that opened issue #7, exactly as filed: a deploy adds a searchable
/// field to an existing collection. It used to be answered `created: false` while
/// `?where=` went on failing, with nothing anywhere to say why.
#[tokio::test]
async fn adding_a_searchable_field_to_a_live_collection_is_refused() {
    let s = state();
    let (st, _) = put(&s, "vehicles", json!({"ttl_seconds": 0})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(stats(&s, "vehicles").await["text"], json!([]));

    let (st, v) = put(&s, "vehicles", json!({"ttl_seconds": 0, "text": ["plate"]})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("config_conflict"));
    assert!(
        v["error"].as_str().unwrap().contains("different text"),
        "the message has to name the field: {v}"
    );
    // And the collection is untouched, which `stats` can now confirm without
    // reading the server's source to find out where to look.
    assert_eq!(stats(&s, "vehicles").await["text"], json!([]));
}

/// Every frozen field, not just the ones someone remembered to wire up. This is
/// the test the old hand-maintained comparison would have failed on four fields.
#[tokio::test]
async fn every_field_the_index_is_built_from_is_a_conflict() {
    let cases: Vec<(&str, Value)> = vec![
        ("max_zoom", json!(15)),
        ("radius", json!(60.0)),
        ("extent", json!(4096.0)),
        ("hysteresis", json!(0.4)),
        (
            "dimensions",
            json!([{"name": "status", "values": ["idle", "enroute", "parked"]}]),
        ),
        ("filters", json!([])),
        ("text", json!(["plate", "driver"])),
    ];
    for (field, value) in cases {
        let s = state();
        let (st, v) = put(&s, "fleet", base()).await;
        assert_eq!(st, StatusCode::OK, "{field}: {v}");
        let (st, v) = put(&s, "fleet", with(field, value)).await;
        assert_eq!(
            st,
            StatusCode::CONFLICT,
            "changing {field} must conflict: {v}"
        );
        assert!(
            v["error"].as_str().unwrap().contains(field),
            "changing {field} must say which field: {v}"
        );
    }
}

/// `categories` needs its own baseline: it and `dimensions` are two spellings of
/// the same thing, and sending both is a 400 before existence is even considered.
#[tokio::test]
async fn changing_the_category_labels_is_a_conflict() {
    let s = state();
    let (st, v) = put(
        &s,
        "fleet",
        json!({"categories": ["idle"], "ttl_seconds": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = put(
        &s,
        "fleet",
        json!({"categories": ["idle", "enroute"], "ttl_seconds": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(v["error"].as_str().unwrap().contains("categories"), "{v}");
}

/// A rejected `PUT` must not half-apply. Otherwise a deploy that gets a 409 for
/// its `text` change would still have silently moved the TTL, which is the worst
/// of both answers.
#[tokio::test]
async fn a_conflict_adopts_nothing() {
    let s = state();
    put(&s, "fleet", base()).await;
    let mut body = with("text", json!(["plate", "driver"]));
    body["ttl_seconds"] = json!(99);
    let (st, _) = put(&s, "fleet", body).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(stats(&s, "fleet").await["ttl_seconds"], json!(0));
}

// ---------------------------------------------------------------- adopted --

#[tokio::test]
async fn a_new_ttl_is_adopted_and_sweeps_by_it() {
    let s = state();
    put(&s, "fleet", json!({"ttl_seconds": 3600})).await;
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(json!({"points": [{"id": "v1", "lng": -46.63, "lat": -23.55}]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let c = collection(&s, "fleet");
    // Old enough to be swept under the TTL that is about to arrive, and nowhere
    // near old enough under the one in force.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(c.sweep(), 0, "an hour has not passed");

    let (st, v) = put(&s, "fleet", json!({"ttl_seconds": 1})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["created"], json!(false));
    assert_eq!(v["adopted"], json!(["ttl_seconds"]));
    assert_eq!(v["collection"]["ttl_seconds"], json!(1));
    assert_eq!(stats(&s, "fleet").await["ttl_seconds"], json!(1));

    assert_eq!(
        c.sweep(),
        1,
        "the adopted TTL has to be the one that sweeps"
    );
}

#[tokio::test]
async fn a_new_props_limit_is_adopted_and_gates_the_next_report() {
    let s = state();
    put(
        &s,
        "fleet",
        json!({"ttl_seconds": 0, "max_props_bytes": 16}),
    )
    .await;
    let report = json!({"points": [
        {"id": "v1", "lng": -46.63, "lat": -23.55,
         "props": {"plate": "abc1234", "driver": "someone"}}
    ]});
    let (st, v) = call(
        &s,
        "POST",
        "/v1/collections/fleet/positions",
        Some(report.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "16 bytes is not enough: {v}");

    let (st, v) = put(
        &s,
        "fleet",
        json!({"ttl_seconds": 0, "max_props_bytes": 4096}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["adopted"], json!(["max_props_bytes"]));
    assert_eq!(stats(&s, "fleet").await["max_props_bytes"], json!(4096));

    let (st, v) = call(&s, "POST", "/v1/collections/fleet/positions", Some(report)).await;
    assert_eq!(st, StatusCode::OK, "the new limit has to apply: {v}");
    assert_eq!(v["accepted"], json!(1));
}

#[tokio::test]
async fn both_limits_can_move_at_once() {
    let s = state();
    put(&s, "fleet", base()).await;
    let mut body = base();
    body["ttl_seconds"] = json!(60);
    body["max_props_bytes"] = json!(2048);
    let (st, v) = put(&s, "fleet", body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["adopted"], json!(["ttl_seconds", "max_props_bytes"]));
}

/// An adopted limit has to outlive a restart. Recording the declaration instead
/// would quietly put the old TTL back on the next boot -- the same silent
/// reversion in a slower disguise.
#[test]
fn an_adopted_limit_survives_a_snapshot() {
    use netcluster_server::collection::Config;
    let dir = std::env::temp_dir().join(format!("netcluster-reconfigure-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fleet.ncsnap");

    let c = Collection::new(
        "fleet",
        Config {
            ttl_seconds: 300,
            max_props_bytes: 512,
            ..Default::default()
        },
    );
    assert_eq!(
        c.adopt(&Config {
            ttl_seconds: 604_800,
            max_props_bytes: 4096,
            ..Default::default()
        }),
        vec!["ttl_seconds", "max_props_bytes"]
    );
    c.snapshot_to(&path).unwrap();

    let (meta, records) = netcluster_server::snapshot::read(&path).unwrap();
    assert_eq!(meta.config.ttl_seconds, 604_800);
    assert_eq!(meta.config.max_props_bytes, 4096);
    let (restored, _) = Collection::restore(&meta.name, meta.config, &meta.labels, &records);
    assert_eq!(restored.ttl_seconds(), 604_800);
    assert_eq!(restored.max_props_bytes(), 4096);

    std::fs::remove_dir_all(&dir).ok();
}
