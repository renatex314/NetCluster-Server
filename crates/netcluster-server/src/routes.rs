//! The HTTP surface.
//!
//! Two output formats, deliberately. `/clusters` returns GeoJSON in the exact
//! shape supercluster produces, so code written against supercluster works
//! unchanged. `/tiles/{z}/{x}/{y}.mvt` returns Mapbox Vector Tiles, which MapLibre
//! and Leaflet consume natively -- the browser then runs *no clustering code at
//! all*, and because a tile key is stable, an HTTP cache in front of this actually
//! hits. At coarse zooms one query serves every viewer looking at that region.

use crate::collection::{Collection, Config, OutFeature, Report};
use crate::mvt;
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub struct AppState {
    pub collections: RwLock<HashMap<String, Arc<Collection>>>,
    pub started_ms: u64,
    /// Create a collection on first write with default settings. Convenient in
    /// development; in production you usually want the config to be explicit.
    pub auto_create: bool,
    pub requests: AtomicU64,
    /// Where snapshots live, when persistence is on.
    pub data_dir: Option<std::path::PathBuf>,
}

impl AppState {
    pub fn get(&self, name: &str) -> Result<Arc<Collection>, ApiError> {
        self.collections
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| {
                ApiError::not_found(format!("no collection {name:?}")).code("no_such_collection")
            })
    }
}

pub struct ApiError(StatusCode, String, &'static str);

impl ApiError {
    fn bad(m: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, m.into(), "bad_request")
    }
    fn not_found(m: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, m.into(), "not_found")
    }
    fn conflict(m: impl Into<String>) -> Self {
        ApiError(StatusCode::CONFLICT, m.into(), "conflict")
    }
    /// A stable slug beside the human message.
    ///
    /// Needed because two very different situations share a status: a device
    /// that is not registered, and a collection that does not exist. A client
    /// deciding between "return false" and "raise" cannot tell those apart from
    /// 404 alone, and matching on the prose would break the first time it is
    /// reworded.
    fn code(mut self, c: &'static str) -> Self {
        self.2 = c;
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1, "code": self.2 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ------------------------------------------------------------------ router --

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/", get(demo_page))
        .route("/v1/collections", get(list_collections))
        .route(
            "/v1/collections/{name}",
            put(create_collection).delete(drop_collection).get(stats),
        )
        .route(
            "/v1/collections/{name}/positions",
            axum::routing::post(positions),
        )
        .route(
            "/v1/collections/{name}/devices/{id}",
            get(get_device).delete(delete_device),
        )
        .route(
            "/v1/collections/{name}/devices/{id}/cluster",
            get(device_cluster),
        )
        .route("/v1/collections/{name}/clusters", get(clusters))
        .route(
            "/v1/collections/{name}/clusters/{cid}/children",
            get(cluster_children),
        )
        .route(
            "/v1/collections/{name}/clusters/{cid}/leaves",
            get(cluster_leaves),
        )
        .route("/v1/collections/{name}/tiles/{z}/{x}/{y}", get(tile))
        .route("/v1/collections/{name}/verify", get(verify))
        .route(
            "/v1/collections/{name}/snapshot",
            axum::routing::post(force_snapshot),
        )
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

/// Permissive CORS, so a map in a browser can talk to this directly. If you put
/// it behind a gateway that sets its own, drop this layer.
async fn cors(req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        add_cors(&mut r);
        return r;
    }
    let mut r = next.run(req).await;
    add_cors(&mut r);
    r
}

fn add_cors(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
}

// ------------------------------------------------------------------- admin --

async fn healthz(State(s): State<Arc<AppState>>) -> Json<Value> {
    let cs = s.collections.read().unwrap();
    let devices: usize = cs.values().map(|c| c.len()).sum();
    Json(json!({
        "status": "ok",
        "collections": cs.len(),
        "devices": devices,
        "uptime_ms": crate::collection::now_ms().saturating_sub(s.started_ms),
        "persistence": s.data_dir.is_some(),
    }))
}

/// Prometheus text exposition. Hand-written rather than pulled from a crate: it is
/// a dozen lines and this way there is no registry to keep in sync.
async fn metrics(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mut out = String::new();
    out.push_str("# HELP netcluster_devices Devices currently in the index\n");
    out.push_str("# TYPE netcluster_devices gauge\n");
    let cs = s.collections.read().unwrap();
    for c in cs.values() {
        let st = c.stats();
        let n = &st.name;
        out.push_str(&format!(
            "netcluster_devices{{collection=\"{n}\"}} {}\n",
            st.devices
        ));
        out.push_str(&format!(
            "netcluster_reports_total{{collection=\"{n}\"}} {}\n",
            st.ingested
        ));
        out.push_str(&format!(
            "netcluster_queries_total{{collection=\"{n}\"}} {}\n",
            st.queries
        ));
        out.push_str(&format!(
            "netcluster_expired_total{{collection=\"{n}\"}} {}\n",
            st.expired
        ));
        out.push_str(&format!(
            "netcluster_memory_bytes{{collection=\"{n}\"}} {}\n",
            st.memory_bytes
        ));
        out.push_str(&format!(
            "netcluster_fast_move_ratio{{collection=\"{n}\"}} {:.4}\n",
            st.moves_fast_pct / 100.0
        ));
        // A snapshot that has quietly stopped succeeding is the failure you want
        // to hear about before you need the data, not after.
        out.push_str(&format!(
            "netcluster_snapshot_last_success_timestamp{{collection=\"{n}\"}} {}\n",
            st.last_snapshot_ms / 1000
        ));
        out.push_str(&format!(
            "netcluster_snapshot_bytes{{collection=\"{n}\"}} {}\n",
            st.last_snapshot_bytes
        ));
        out.push_str(&format!(
            "netcluster_snapshot_failures_total{{collection=\"{n}\"}} {}\n",
            st.snapshot_failures
        ));
        out.push_str(&format!(
            "netcluster_restored_devices{{collection=\"{n}\"}} {}\n",
            st.restored
        ));
    }
    out.push_str(&format!(
        "netcluster_requests_total {}\n",
        s.requests.load(Ordering::Relaxed)
    ));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}

async fn demo_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../demo/index.html"),
    )
}

// -------------------------------------------------------------- collections --

#[derive(Deserialize, Default)]
pub struct ConfigBody {
    pub max_zoom: Option<u8>,
    pub radius: Option<f64>,
    pub extent: Option<f64>,
    pub hysteresis: Option<f64>,
    /// Category labels; a label's position in this list is its category index.
    pub categories: Option<Vec<String>>,
    /// Largest per-device properties blob accepted, in bytes. 0 refuses properties.
    pub max_props_bytes: Option<usize>,
    pub ttl_seconds: Option<u64>,
}

impl ConfigBody {
    fn into_config(self) -> Config {
        let d = Config::default();
        Config {
            max_zoom: self.max_zoom.unwrap_or(d.max_zoom),
            radius: self.radius.unwrap_or(d.radius),
            extent: self.extent.unwrap_or(d.extent),
            hysteresis: self.hysteresis.unwrap_or(d.hysteresis),
            categories: self.categories.unwrap_or(d.categories),
            max_props_bytes: self.max_props_bytes.unwrap_or(d.max_props_bytes),
            ttl_seconds: self.ttl_seconds.unwrap_or(d.ttl_seconds),
        }
    }
}

async fn list_collections(State(s): State<Arc<AppState>>) -> Json<Value> {
    let cs = s.collections.read().unwrap();
    let mut names: Vec<_> = cs.values().map(|c| c.stats()).collect();
    names.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "collections": names }))
}

async fn create_collection(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Option<Json<ConfigBody>>,
) -> ApiResult<Json<Value>> {
    let cfg = body.map(|Json(b)| b).unwrap_or_default().into_config();
    if cfg.max_zoom > 20 {
        return Err(ApiError::bad("max_zoom must be <= 20"));
    }
    if cfg.radius <= 0.0 || cfg.extent <= 0.0 {
        return Err(ApiError::bad("radius and extent must be positive"));
    }
    let mut cs = s.collections.write().unwrap();
    if let Some(existing) = cs.get(&name) {
        // Idempotent for the same geometry, an error for a different one. Silently
        // keeping the old geometry would mean two deployments disagreeing about
        // what a cluster means while both believe they configured it.
        let e = &existing.config;
        if e.max_zoom != cfg.max_zoom
            || e.radius != cfg.radius
            || e.extent != cfg.extent
            || e.categories != cfg.categories
        {
            return Err(ApiError::conflict(format!(
                "collection {name:?} already exists with a different geometry; \
                 drop it or use another name"
            )));
        }
        return Ok(Json(
            json!({ "created": false, "collection": existing.stats() }),
        ));
    }
    let c = Arc::new(Collection::new(&name, cfg));
    let st = c.stats();
    cs.insert(name, c);
    Ok(Json(json!({ "created": true, "collection": st })))
}

async fn drop_collection(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let dropped = s.collections.write().unwrap().remove(&name);
    match dropped {
        Some(c) => {
            // The snapshot has to go with it, or the collection resurrects at the
            // next restart and a delete quietly did not stick.
            let mut file_removed = false;
            if let Some(dir) = &s.data_dir {
                match crate::snapshot::remove(&crate::snapshot::path_for(dir, &name)) {
                    Ok(()) => file_removed = true,
                    Err(e) => eprintln!("[snapshot] could not delete {name}: {e}"),
                }
            }
            Ok(Json(json!({
                "dropped": name,
                "devices": c.len(),
                "snapshot_removed": file_removed,
            })))
        }
        None => {
            Err(ApiError::not_found(format!("no collection {name:?}")).code("no_such_collection"))
        }
    }
}

async fn stats(State(s): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.get(&name)?.stats())))
}

/// Write a snapshot now.
///
/// Worth having before a deliberate restart, where waiting out the interval would
/// otherwise lose whatever arrived since the last one.
async fn force_snapshot(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(dir) = s.data_dir.clone() else {
        return Err(ApiError::bad(
            "persistence is off; start the server with NETCLUSTER_DATA_DIR set",
        )
        .code("persistence_disabled"));
    };
    let c = s.get(&name)?;
    let path = crate::snapshot::path_for(&dir, &name);
    // Serialising and writing must not happen on a runtime worker.
    let bytes = tokio::task::spawn_blocking(move || c.snapshot_to(&path))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), "panic"))?
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("snapshot failed: {e}"),
                "snapshot_failed",
            )
        })?;
    Ok(Json(json!({ "snapshot": name, "bytes": bytes })))
}

async fn verify(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    match s.get(&name)?.verify() {
        Ok(v) => Ok(Json(json!({ "ok": true, "detail": v }))),
        Err(e) => Ok(Json(json!({ "ok": false, "violation": e }))),
    }
}

// ------------------------------------------------------------------ ingest --

#[derive(Deserialize)]
#[serde(untagged)]
enum CatVal {
    Num(u32),
    Name(String),
}

/// `deny_unknown_fields` on purpose. Free-form attributes go in `props`; a stray
/// `"plate"` at the top level is a mistake, and silently discarding it means
/// finding out weeks later that nothing was ever stored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBody {
    id: String,
    lng: f64,
    lat: f64,
    #[serde(default)]
    cat: Option<CatVal>,
    /// Any JSON object. Omit it to leave the device's existing properties alone;
    /// send `{}` to clear them.
    #[serde(default)]
    props: Option<Box<serde_json::value::RawValue>>,
}

/// Either a bare array of reports or `{ "points": [...] }`.
///
/// Hand-written rather than `#[serde(untagged)]`, which cannot work here: an
/// untagged enum deserialises by buffering the input into an intermediate
/// representation and retrying each variant, and that buffer discards the original
/// text -- which is exactly what `RawValue` needs to capture `props` without
/// parsing. The derived version accepted both shapes right up until a report
/// carried properties, then failed with "data did not match any variant".
struct PositionsBody(Vec<ReportBody>);

impl<'de> Deserialize<'de> for PositionsBody {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PositionsBody;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of position reports, or an object with a `points` array")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut v = Vec::with_capacity(a.size_hint().unwrap_or(0));
                while let Some(r) = a.next_element()? {
                    v.push(r);
                }
                Ok(PositionsBody(v))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut points: Option<Vec<ReportBody>> = None;
                while let Some(k) = a.next_key::<String>()? {
                    if k == "points" {
                        points = Some(a.next_value()?);
                    } else {
                        return Err(serde::de::Error::unknown_field(&k, &["points"]));
                    }
                }
                points
                    .map(PositionsBody)
                    .ok_or_else(|| serde::de::Error::missing_field("points"))
            }
        }
        d.deserialize_any(V)
    }
}

async fn positions(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<PositionsBody>,
) -> ApiResult<Json<Value>> {
    let reports = body.0;
    if reports.is_empty() {
        return Ok(Json(json!({ "accepted": 0 })));
    }

    let c = match s.get(&name) {
        Ok(c) => c,
        Err(e) => {
            if !s.auto_create {
                return Err(e);
            }
            let mut cs = s.collections.write().unwrap();
            cs.entry(name.clone())
                .or_insert_with(|| Arc::new(Collection::new(&name, Config::default())))
                .clone()
        }
    };

    let mut resolved = Vec::with_capacity(reports.len());
    for r in &reports {
        let cat = match &r.cat {
            None => 0,
            Some(CatVal::Num(n)) => *n,
            Some(CatVal::Name(s)) => c.category(Some(s)).map_err(ApiError::bad)?.max(0) as u32,
        };
        resolved.push(Report {
            id: &r.id,
            lng: r.lng,
            lat: r.lat,
            cat,
            props: r.props.as_deref(),
        });
    }
    let n = c.upsert(&resolved).map_err(ApiError::bad)?;
    Ok(Json(json!({ "accepted": n, "devices": c.len() })))
}

/// Is this device registered, and what does the index know about it?
///
/// 200 with the record, 404 if it is not registered -- so a bare existence check
/// is a HEAD against this route, which axum serves from the same handler without
/// a body.
async fn get_device(
    State(s): State<Arc<AppState>>,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    match c.device(&id) {
        Some(d) => Ok(Json(json!(d))),
        None => Err(
            ApiError::not_found(format!("device {id:?} is not registered in {name:?}"))
                .code("device_not_registered"),
        ),
    }
}

async fn delete_device(
    State(s): State<Arc<AppState>>,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    Ok(Json(json!({ "removed": c.remove(&id) })))
}

// ------------------------------------------------------------------ query --

fn parse_bbox(q: &HashMap<String, String>) -> ApiResult<[f64; 4]> {
    let Some(raw) = q.get("bbox") else {
        return Ok([-180.0, -85.0511, 180.0, 85.0511]);
    };
    let v: Vec<f64> = raw
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| ApiError::bad("bbox must be four numbers: west,south,east,north"))?;
    if v.len() != 4 || v.iter().any(|f| !f.is_finite()) {
        return Err(ApiError::bad(
            "bbox must be four finite numbers: west,south,east,north",
        ));
    }
    Ok([v[0], v[1], v[2], v[3]])
}

fn parse_zoom(q: &HashMap<String, String>) -> ApiResult<f64> {
    match q.get("zoom") {
        None => Ok(0.0),
        Some(z) => z
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .ok_or_else(|| ApiError::bad("zoom must be a number")),
    }
}

fn abbrev(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else if n >= 1000 {
        format!("{}k", (n as f64 / 100.0).round() / 10.0)
    } else {
        n.to_string()
    }
}

/// GeoJSON in the shape supercluster emits, so existing client code works.
fn geojson(f: &OutFeature) -> Value {
    if let Some(dev) = &f.device {
        // The device's own properties when it has any, matching what the
        // JavaScript library returns. The id is still on the feature itself, so
        // nothing is lost by handing the object over verbatim.
        let properties = match &f.props {
            Some(p) => json!(p),
            None => json!({ "id": dev }),
        };
        json!({
            "type": "Feature",
            "id": dev,
            "properties": properties,
            "geometry": { "type": "Point", "coordinates": [f.lng, f.lat] }
        })
    } else {
        json!({
            "type": "Feature",
            "properties": {
                "cluster": true,
                "cluster_id": f.cluster_id,
                "point_count": f.count,
                "point_count_abbreviated": abbrev(f.count),
            },
            "geometry": { "type": "Point", "coordinates": [f.lng, f.lat] }
        })
    }
}

fn collection_json(fs: &[OutFeature]) -> Value {
    json!({
        "type": "FeatureCollection",
        "features": fs.iter().map(geojson).collect::<Vec<_>>(),
    })
}

async fn clusters(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    let cat = c
        .category(q.get("cat").map(|s| s.as_str()))
        .map_err(ApiError::bad)?;
    let fs = c.clusters(parse_bbox(&q)?, parse_zoom(&q)?, cat);
    Ok(Json(collection_json(&fs)))
}

async fn device_cluster(
    State(s): State<Arc<AppState>>,
    Path((name, id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    let z = parse_zoom(&q)? as i32;
    match c.device_cluster(&id, z) {
        Some(f) => Ok(Json(geojson(&f))),
        None => Err(
            ApiError::not_found(format!("device {id:?} is not in {name:?}"))
                .code("device_not_registered"),
        ),
    }
}

async fn cluster_children(
    State(s): State<Arc<AppState>>,
    Path((name, cid)): Path<(String, u64)>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    let fs = c.children(cid).map_err(ApiError::bad)?;
    let nz = c.expansion_zoom(cid).map_err(ApiError::bad)?;
    let mut v = collection_json(&fs);
    v["expansion_zoom"] = json!(nz);
    Ok(Json(v))
}

async fn cluster_leaves(
    State(s): State<Arc<AppState>>,
    Path((name, cid)): Path<(String, u64)>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let c = s.get(&name)?;
    let limit = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10usize);
    let offset = q
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0usize);
    let fs = c
        .leaves(cid, limit.min(10_000), offset)
        .map_err(ApiError::bad)?;
    Ok(Json(collection_json(&fs)))
}

async fn tile(
    State(s): State<Arc<AppState>>,
    Path((name, z, x, y_ext)): Path<(String, i32, i64, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let (y_str, ext) = match y_ext.rsplit_once('.') {
        Some((y, e)) => (y, e),
        None => (y_ext.as_str(), "mvt"),
    };
    let y: i64 = y_str
        .parse()
        .map_err(|_| ApiError::bad("tile y must be an integer"))?;
    if !(0..=20).contains(&z) {
        return Err(ApiError::bad("tile z must be between 0 and 20"));
    }
    let side = 1i64 << z;
    if x < 0 || x >= side || y < 0 || y >= side {
        return Err(ApiError::bad(format!(
            "tile ({x}, {y}) is outside the 0..{side} range for zoom {z}"
        )));
    }
    let c = s.get(&name)?;
    let cat = c
        .category(q.get("cat").map(|s| s.as_str()))
        .map_err(ApiError::bad)?;
    let feats = c.tile(z, x, y, cat);

    match ext {
        "mvt" | "pbf" => {
            let extent = c.config.extent as u32;
            let mut layer = mvt::Layer::new("clusters", extent);
            for f in &feats {
                if let Some(dev) = &f.device {
                    let mut tags = vec![
                        ("cluster", mvt::Val::Bool(false)),
                        ("id", mvt::Val::Str(dev.clone())),
                    ];
                    // Top-level scalars become tags so a renderer can style by
                    // them. Nested objects and arrays are skipped rather than
                    // stringified: a vector-tile value is a scalar, and quietly
                    // turning {"a":1} into the text `{"a":1}` would produce a
                    // filter that silently never matches.
                    let flat = f.props.as_ref().and_then(|p| {
                        serde_json::from_str::<serde_json::Map<String, Value>>(p.get()).ok()
                    });
                    if let Some(map) = &flat {
                        for (k, v) in map {
                            let val = match v {
                                Value::String(x) => mvt::Val::Str(x.clone()),
                                Value::Bool(b) => mvt::Val::Bool(*b),
                                Value::Number(n) if n.is_u64() => {
                                    mvt::Val::Uint(n.as_u64().unwrap())
                                }
                                Value::Number(n) => mvt::Val::Str(n.to_string()),
                                _ => continue,
                            };
                            if k != "cluster" && k != "id" {
                                tags.push((k.as_str(), val));
                            }
                        }
                    }
                    layer.add_point(f.id, f.x, f.y, &tags);
                } else {
                    layer.add_point(
                        f.id,
                        f.x,
                        f.y,
                        &[
                            ("cluster", mvt::Val::Bool(true)),
                            ("point_count", mvt::Val::Uint(f.count as u64)),
                            ("point_count_abbreviated", mvt::Val::Str(abbrev(f.count))),
                        ],
                    );
                }
            }
            let body = if layer.is_empty() {
                Vec::new()
            } else {
                mvt::encode(vec![layer])
            };
            Ok((
                [
                    (header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile"),
                    // Data moves constantly, so the window is short -- but at coarse
                    // zooms a tile is shared by every viewer looking at that region,
                    // and even two seconds collapses thousands of queries into one.
                    (header::CACHE_CONTROL, "public, max-age=2"),
                ],
                body,
            )
                .into_response())
        }
        "json" | "geojson" => {
            let fs: Vec<OutFeature> = feats
                .iter()
                .map(|f| OutFeature {
                    lng: f.x as f64,
                    lat: f.y as f64,
                    count: f.count,
                    device: f.device.clone(),
                    cluster_id: if f.device.is_some() { None } else { Some(f.id) },
                    props: f.props.clone(),
                })
                .collect();
            let mut v = collection_json(&fs);
            v["note"] = json!("coordinates are tile-extent units, not degrees");
            v["extent"] = json!(c.config.extent);
            Ok(Json(v).into_response())
        }
        other => Err(ApiError::bad(format!(
            "unknown tile format {other:?}; use .mvt or .json"
        ))),
    }
}
