//! A collection: one index, plus the bookkeeping a server needs around it.
//!
//! # What this is not
//!
//! It is not a database. It holds no truth: the authority for where your devices
//! are lives wherever the position reports come from, and this is a *materialised
//! view* of that stream. That single fact removes the entire durability chapter --
//! no write-ahead log, no snapshot format, no compaction, no replication protocol,
//! no failover, no split-brain. A process that dies is a process you restart, and
//! at roughly a microsecond per insert a 500,000-device fleet is back in about a
//! second.
//!
//! It also means the scaling model is replication, not sharding: run N identical
//! processes, feed them all the same stream, query any of them. There is nothing
//! to coordinate because there is nothing to protect.
//!
//! # Why not shard geographically
//!
//! Because you cannot. An ordinary spatial index can be split by region, since an
//! R-tree or grid query is spatially local. This hierarchy is *globally coupled at
//! coarse zooms* -- a cluster at z=0 spans continents, so a vehicle in Brazil and
//! one in Angola can share a parent. Split the world in two and the coarse zooms
//! are wrong. Shard by collection (fleet A, fleet B), never by region, and size a
//! process so that one collection fits in it.

use crate::schema::{Dimension, Interner, Interning, Looking, Schema};
use crate::snapshot::DeviceRecord;

/// The cell of a query that cannot match anything. Distinct from -1, which means
/// "no filter at all" -- confusing the two would show the whole fleet.
pub const NO_MATCH: i32 = -2;
use netcluster::{Feature, NetCluster, Options};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How many devices to drop per write-lock acquisition during an expiry sweep.
///
/// The sweep is the only operation that can want the write lock for a long time,
/// and while it holds it every query blocks. Chunking turns one long stall into
/// many short ones. 256 removals is about half a millisecond.
const SWEEP_CHUNK: usize = 256;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub max_zoom: u8,
    pub radius: f64,
    pub extent: f64,
    pub hysteresis: f64,
    /// Category labels. The index of a label *is* its category, so a query can say
    /// `?cat=delivering` instead of `?cat=2`.
    ///
    /// A shorthand for a single dimension named `cat`. Set this or `dimensions`,
    /// never both.
    pub categories: Vec<String>,
    /// Properties this collection can filter on. Each declares its possible
    /// values, and `multi` lets one device hold several at once -- a vehicle owned
    /// by three clients, say, which a single category cannot express.
    #[serde(default)]
    pub dimensions: Vec<Dimension>,
    /// Which combinations of dimensions a query may name. Empty means each
    /// dimension on its own.
    ///
    /// This is what filtering costs: a device contributes one aggregate entry per
    /// shape per tree level, so declaring `[["client"], ["status"],
    /// ["client","status"]]` costs three times what `[["client"]]` does.
    #[serde(default)]
    pub filters: Vec<Vec<String>>,
    /// Largest per-device properties blob accepted, in bytes. 0 refuses properties
    /// entirely.
    ///
    /// Memory is bounded by devices times this number, so it is a real limit and
    /// not a formality: at a million devices, every kilobyte allowed here is a
    /// gigabyte you have promised to have.
    pub max_props_bytes: usize,
    /// Property fields that can be searched with `?where=`.
    ///
    /// A substring cannot be pre-aggregated -- there is nothing to keep a running
    /// count of -- so a `where` query scans. Each declared field costs one string
    /// per device, extracted from `props` at ingest so the scan never parses JSON,
    /// and lowercased once so matching never allocates.
    #[serde(default)]
    pub text: Vec<String>,
    /// Drop a device that has not reported for this long. 0 disables expiry.
    ///
    /// You almost always want this set. A vehicle that stops reporting does not
    /// stop existing in the index, and clusters quietly fill with ghosts.
    pub ttl_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_zoom: 16,
            radius: 40.0,
            extent: 512.0,
            hysteresis: 0.25,
            categories: Vec::new(),
            dimensions: Vec::new(),
            filters: Vec::new(),
            text: Vec::new(),
            max_props_bytes: 1024,
            ttl_seconds: 300,
        }
    }
}

/// Device ids arrive as strings and the index wants integers, so they are
/// interned. Interning is permanent: an id that goes away keeps its number, so a
/// device that reappears lands back in the same slot. For a fleet, where ids are
/// stable, the table is bounded by the number of distinct devices ever seen.
#[derive(Default)]
struct IdMap {
    to_num: HashMap<String, u64>,
    to_str: Vec<String>,
    /// Last report time per interned id; `u64::MAX` means "not currently live".
    last_seen: Vec<u64>,
    /// Optional source version for rejecting late retries. `None` preserves the
    /// arrival-order semantics until a device first receives a version.
    last_update: Vec<Option<u64>>,
    /// Last projected position, kept outside the index so it can be rebuilt if a
    /// long-lived incremental update ever fails to land.
    positions: Vec<Option<(i32, i32)>>,
    /// Last resolved filter cells, kept as source data for the same repair path.
    cells: Vec<Vec<u32>>,
    /// Free-form properties per interned id, as raw JSON.
    ///
    /// Held behind an `Arc` so a query copies a refcount rather than the text: at
    /// max zoom a viewport can return tens of thousands of single points, and
    /// cloning each blob would dominate the query.
    props: Vec<Option<Arc<Box<RawValue>>>>,
    /// Searchable fields, `device * fields + field`, lowercased.
    ///
    /// Held apart from `props` so a scan touches a compact array of short strings
    /// rather than parsing a JSON blob per device, and lowercased at ingest so a
    /// query allocates nothing per device it rejects.
    text: Vec<Option<Box<str>>>,
    fields: usize,
}

impl IdMap {
    fn intern(&mut self, id: &str) -> u64 {
        if let Some(&n) = self.to_num.get(id) {
            return n;
        }
        let n = self.to_str.len() as u64;
        self.to_str.push(id.to_string());
        self.last_seen.push(u64::MAX);
        self.last_update.push(None);
        self.positions.push(None);
        self.cells.push(Vec::new());
        self.props.push(None);
        for _ in 0..self.fields {
            self.text.push(None);
        }
        self.to_num.insert(id.to_string(), n);
        n
    }

    fn name(&self, n: u64) -> &str {
        self.to_str
            .get(n as usize)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }
}

struct Inner {
    index: NetCluster,
    ids: IdMap,
}

/// One `?where=` term: a field, how to match, and what to match against.
#[derive(Debug, Clone)]
pub struct TextPred {
    pub field: usize,
    pub contains: bool,
    /// Already lowercased, like the values it is tested against.
    pub needle: String,
}

impl TextPred {
    fn test(&self, v: Option<&str>) -> bool {
        match v {
            None => false, // a device without the field cannot match
            Some(t) => {
                if self.contains {
                    t.contains(&self.needle)
                } else {
                    t == self.needle
                }
            }
        }
    }
}

pub struct Collection {
    pub name: String,
    pub config: Config,
    /// Resolved filter schema. Derived from `config`, kept beside it so a query
    /// resolves names to a cell without rebuilding it every time.
    pub schema: Schema,
    /// Where a device with no filter values at all goes: value 0 in every
    /// dimension, which is what a missing `category` has always meant. Without
    /// this a device reported without values would hold no cells and vanish from
    /// every filter while still appearing unfiltered.
    default_cells: Vec<u32>,
    /// Value indices for dimensions declared with a `capacity` rather than a list.
    ///
    /// Its own lock rather than the index's: interning happens while resolving a
    /// batch, before the index is touched at all, and a query needs it for a few
    /// microseconds before taking the read lock. The two are never held together,
    /// and always in this order.
    interner: RwLock<Interner>,
    state: RwLock<Inner>,
    /// Async backpressure for report requests. The actual index remains behind
    /// its synchronous lock, but waiting requests must not occupy runtime workers.
    write_gate: Arc<tokio::sync::Mutex<()>>,
    snapshot_gate: std::sync::Mutex<()>,
    live_devices: std::sync::atomic::AtomicUsize,
    pub created_ms: u64,
    pub ingested: AtomicU64,
    pub queries: AtomicU64,
    pub expired: AtomicU64,
    /// Snapshot bookkeeping. A snapshot that has silently stopped succeeding --
    /// full disk, wrong permissions -- is exactly the failure you want to hear
    /// about before you need the data, so it is surfaced in stats and metrics.
    pub last_snapshot_ms: AtomicU64,
    pub last_snapshot_bytes: AtomicU64,
    pub snapshot_failures: AtomicU64,
    /// Devices loaded from a snapshot at startup.
    pub restored: AtomicU64,
    /// Reports ignored because their source version was older than the stored one.
    pub stale_reports: AtomicU64,
    /// Defensive index rebuilds triggered by a position mismatch.
    pub repairs: AtomicU64,
}

/// One position report.
#[derive(Debug, Clone)]
pub struct Report<'a> {
    pub id: &'a str,
    pub lng: f64,
    pub lat: f64,
    /// `None` leaves whatever the device already had; `Some` replaces it.
    ///
    /// That asymmetry is the point: properties are slow-changing metadata and
    /// positions arrive many times a second, so a position report should not have
    /// to resend the number plate to avoid erasing it. To actually clear them,
    /// send an empty object.
    ///
    /// There is no partial update. `Some` replaces the whole object, deliberately:
    /// merge semantics on nested values are ambiguous -- given a stored
    /// `{"nested":{"a":1}}`, a patch of `{"nested":{"b":2}}` could reasonably
    /// replace `nested` or merge into it -- and replacement is not.
    pub props: Option<&'a RawValue>,
    /// The filter cells this device belongs to.
    ///
    /// `None` means the report carried no filter values at all, which leaves the
    /// device's existing ones alone -- a bare position report must not silently
    /// re-file a vehicle into whatever value happens to be index 0.
    pub cells: Option<&'a [u32]>,
    /// Optional source-side monotonic version, in milliseconds since the epoch.
    /// Unversioned reports use arrival order only until the device becomes versioned.
    pub updated_at_ms: Option<u64>,
}

/// One thing to draw.
#[derive(Debug, Clone)]
pub struct OutFeature {
    pub lng: f64,
    pub lat: f64,
    pub count: u32,
    /// The device id, when this feature is a single point.
    pub device: Option<String>,
    /// The cluster handle, when it is not.
    pub cluster_id: Option<u64>,
    /// The device's properties, for single points. A cluster has none: forty
    /// vehicles do not share a battery level.
    pub props: Option<Arc<Box<RawValue>>>,
}

/// Everything the index knows about one device.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub lng: f64,
    pub lat: f64,
    /// The category label if the collection has any, otherwise the raw index.
    pub cat: Option<String>,
    pub cat_index: u32,
    /// When this device last reported, in milliseconds since the epoch.
    pub last_seen_ms: u64,
    /// How long ago that was. The useful form: compare it against the TTL to see
    /// how close a device is to being swept.
    pub age_ms: u64,
    /// Last accepted source-side version, when the client supplied one.
    pub updated_at_ms: Option<u64>,
    /// Whatever was last reported for this device, or null.
    pub props: Option<Arc<Box<RawValue>>>,
}

/// A point placed inside a vector tile.
#[derive(Debug, Clone)]
pub struct OutTileFeature {
    pub x: i32,
    pub y: i32,
    pub count: u32,
    pub id: u64,
    pub device: Option<String>,
    pub props: Option<Arc<Box<RawValue>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CollectionStats {
    pub name: String,
    pub devices: usize,
    pub max_zoom: u8,
    pub radius: f64,
    pub categories: Vec<String>,
    pub ttl_seconds: u64,
    pub memory_bytes: usize,
    pub grid_entries: usize,
    pub centers_per_level: Vec<u32>,
    pub ingested: u64,
    pub queries: u64,
    pub expired: u64,
    pub uptime_ms: u64,
    pub moves_fast_pct: f64,
    /// 0 when no snapshot has been written, or persistence is off.
    pub last_snapshot_ms: u64,
    pub last_snapshot_bytes: u64,
    pub snapshot_failures: u64,
    pub restored: u64,
    pub stale_reports: u64,
    pub repairs: u64,
    /// Number of dynamic values currently interned per dimension.
    pub interned: Vec<usize>,
    /// Declared value capacity per dimension; compare with `interned` to spot
    /// a dimension approaching exhaustion before reports begin failing.
    pub dimension_capacities: Vec<usize>,
    /// Bytes of device properties currently held. Worth watching: it is the one
    /// part of the index whose size you control from outside.
    pub props_bytes: usize,
    pub max_props_bytes: usize,
    /// What the searchable fields cost, so a `?where=` collection can be sized.
    pub text_bytes: usize,
}

impl Config {
    /// Resolve the declared filters, or say why they cannot be.
    ///
    /// `categories` is the older spelling of one dimension named `cat`; it is
    /// translated here so there is exactly one code path below this line.
    pub fn schema(&self) -> Result<Schema, String> {
        if !self.categories.is_empty() && !self.dimensions.is_empty() {
            return Err("set either `categories` or `dimensions`, not both".into());
        }
        if !self.filters.is_empty() && self.dimensions.is_empty() {
            return Err("`filters` needs `dimensions` to name".into());
        }
        let dims = if self.dimensions.is_empty() {
            if self.categories.is_empty() {
                Vec::new()
            } else {
                vec![Dimension {
                    name: "cat".into(),
                    values: self.categories.clone(),
                    capacity: None,
                    multi: false,
                }]
            }
        } else {
            self.dimensions.clone()
        };
        Schema::new(dims, &self.filters)
    }
}

impl Collection {
    pub fn new(name: &str, config: Config) -> Self {
        let schema = config
            .schema()
            .expect("Config::validate must run before Collection::new");
        let index = NetCluster::new(Options {
            min_zoom: 0,
            max_zoom: config.max_zoom,
            radius: config.radius,
            extent: config.extent,
            hysteresis: config.hysteresis,
            cells: schema.cells,
            max_cells_per_device: schema.max_cells_per_device,
            ..Default::default()
        });
        let text_fields = config.text.len();
        let mut interner = Interner::new(&schema);
        let mut default_cells = Vec::new();
        schema
            .cells_for(
                &HashMap::new(),
                &mut default_cells,
                &mut Interning {
                    schema: &schema,
                    interner: &mut interner,
                },
            )
            .expect("value 0 exists in every declared dimension");
        Collection {
            name: name.to_string(),
            schema,
            default_cells,
            interner: RwLock::new(interner),
            config,
            state: RwLock::new(Inner {
                index,
                ids: IdMap {
                    fields: text_fields,
                    ..IdMap::default()
                },
            }),
            write_gate: Arc::new(tokio::sync::Mutex::new(())),
            snapshot_gate: std::sync::Mutex::new(()),
            live_devices: std::sync::atomic::AtomicUsize::new(0),
            created_ms: now_ms(),
            ingested: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            last_snapshot_ms: AtomicU64::new(0),
            last_snapshot_bytes: AtomicU64::new(0),
            snapshot_failures: AtomicU64::new(0),
            restored: AtomicU64::new(0),
            stale_reports: AtomicU64::new(0),
            repairs: AtomicU64::new(0),
        }
    }

    /// Serialize report application without making async runtime workers wait
    /// inside a synchronous mutex. The guard may be held across a blocking task.
    pub async fn acquire_write(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.write_gate.clone().lock_owned().await
    }

    /// Rebuild a collection from a snapshot.
    ///
    /// Inserts the stored fixed-point coordinates directly rather than going back
    /// through the projection: re-projecting would re-round every position on
    /// every restart, and the drift would accumulate over a service's lifetime.
    ///
    /// Returns the collection and how many records were dropped for being older
    /// than the TTL -- a snapshot from long enough ago restores nothing, which is
    /// correct: those devices went quiet and would be swept within seconds anyway.
    pub fn restore(
        name: &str,
        config: Config,
        labels: &[Vec<String>],
        records: &[DeviceRecord],
    ) -> (Self, usize) {
        let c = Collection::new(name, config);
        // Before any record: the cells about to be restored are indices into this
        // table, so it has to be the one that produced them.
        *c.interner.write().unwrap() = Interner::restore(&c.schema, labels);
        let cutoff = if c.config.ttl_seconds > 0 {
            now_ms().saturating_sub(c.config.ttl_seconds * 1000)
        } else {
            0
        };
        let cells = c.schema.cells;
        let mut skipped = 0usize;
        {
            let mut st = c.state.write().unwrap();
            for r in records {
                if r.last_seen_ms < cutoff {
                    skipped += 1;
                    continue;
                }
                // The config can have changed since the snapshot was written, so
                // a cell that no longer exists is dropped rather than panicking on
                // a startup path, where a panic means the process never comes back
                // at all. Dropping is the safe direction: the device still appears
                // on the map, just not under a filter it can no longer belong to.
                let mut kept: Vec<u32> = r
                    .cells
                    .iter()
                    .copied()
                    .filter(|&c| (c as usize) < cells)
                    .collect();
                kept.truncate(c.schema.max_cells_per_device);
                // If the declaration changed enough that nothing survived, the
                // device still has to land somewhere filterable rather than
                // disappearing from every filter while showing up unfiltered.
                if kept.is_empty() {
                    kept.extend_from_slice(&c.default_cells);
                }
                let n = st.ids.intern(&r.id);
                st.index.insert_projected_cells(n, r.x, r.y, &kept);
                st.ids.last_seen[n as usize] = r.last_seen_ms;
                st.ids.last_update[n as usize] = r.updated_at_ms;
                st.ids.positions[n as usize] = Some((r.x, r.y));
                st.ids.cells[n as usize] = kept;
                if let Some(p) = &r.props {
                    st.ids.props[n as usize] = RawValue::from_string(p.clone()).ok().map(Arc::new);
                    if !c.config.text.is_empty() {
                        if let Ok(mut got) = crate::geojson::peek_text(
                            p,
                            &c.config.text.iter().map(String::as_str).collect::<Vec<_>>(),
                        ) {
                            for t in got.iter_mut().flatten() {
                                *t = t.to_lowercase();
                            }
                            let base = n as usize * st.ids.fields;
                            for (f, v) in got.iter().enumerate() {
                                st.ids.text[base + f] = v.as_deref().map(Box::from);
                            }
                        }
                    }
                }
            }
        }
        c.live_devices
            .store(c.state.read().unwrap().index.len(), Ordering::Relaxed);
        c.restored
            .store(c.len() as u64, std::sync::atomic::Ordering::Relaxed);
        (c, skipped)
    }

    /// Every live device, as stored.
    ///
    /// The read lock covers the copy and nothing else. A read lock blocks the
    /// writer, so serialising tens of megabytes inside it would stall ingest for
    /// the duration of the write -- the caller serialises afterwards.
    pub fn export(&self) -> Vec<DeviceRecord> {
        let st = self.state.read().unwrap();
        let mut out = Vec::with_capacity(st.index.len());
        for (n, &seen) in st.ids.last_seen.iter().enumerate() {
            if seen == u64::MAX {
                continue; // interned once, not currently live
            }
            let n = n as u64;
            let Some((x, y)) = st.ids.positions[n as usize] else {
                continue;
            };
            out.push(DeviceRecord {
                id: st.ids.to_str[n as usize].clone(),
                x,
                y,
                cells: st.ids.cells[n as usize].clone(),
                last_seen_ms: seen,
                updated_at_ms: st.ids.last_update[n as usize],
                props: st.ids.props[n as usize]
                    .as_ref()
                    .map(|p| p.get().to_owned()),
            });
        }
        out
    }

    /// Export and write a snapshot, recording the outcome for /metrics.
    pub fn snapshot_to(&self, path: &std::path::Path) -> std::io::Result<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        let _snapshot_guard = self.snapshot_gate.lock().unwrap();
        let records = self.export();
        let meta = crate::snapshot::Meta {
            name: self.name.clone(),
            config: self.config.clone(),
            labels: self.interner.read().unwrap().labels(),
        };
        match crate::snapshot::write(path, &meta, &records) {
            Ok(n) => {
                self.last_snapshot_ms.store(now_ms(), Relaxed);
                self.last_snapshot_bytes.store(n, Relaxed);
                Ok(n)
            }
            Err(e) => {
                self.snapshot_failures.fetch_add(1, Relaxed);
                Err(e)
            }
        }
    }

    /// Resolve a category selector: either a label from the config, or a plain
    /// index. Returns `Err` for a selector that names nothing, so a typo in a
    /// query string fails loudly instead of silently returning an empty map.
    pub fn category(&self, sel: Option<&str>) -> Result<i32, String> {
        let Some(sel) = sel else { return Ok(-1) };
        if sel.is_empty() {
            return Ok(-1);
        }
        let mut one = HashMap::with_capacity(1);
        let name = match self.schema.dims.first() {
            Some(d) => d.name.clone(),
            None => {
                return Err(format!(
                    "unknown category {sel:?}; this collection declares no filters"
                ))
            }
        };
        one.insert(name, sel.to_string());
        // Reworded rather than passed through: `?cat=` is the published spelling
        // and its error text is what clients match on. The schema underneath calls
        // the same thing a dimension value.
        match self.filter_cell(&one) {
            Ok(Some(c)) => Ok(c),
            // `categories` are always declared, so "never seen" cannot arise here
            Ok(None) => Ok(NO_MATCH),
            Err(_) => Err(format!(
                "unknown category {sel:?}; this collection has {:?}",
                self.config.categories
            )),
        }
    }

    /// The cell a `?f.name=value` selection picks.
    ///
    /// `Ok(None)` means no device can match: the query named a value on a dynamic
    /// dimension that nothing has ever reported. That is an empty map rather than
    /// an error -- on a dimension whose values are discovered as devices arrive,
    /// a caller cannot know which exist yet.
    pub fn filter_cell(&self, sel: &HashMap<String, String>) -> Result<Option<i32>, String> {
        let interner = self.interner.read().unwrap();
        self.schema.query_cell(
            sel,
            &mut Looking {
                schema: &self.schema,
                interner: &interner,
            },
        )
    }

    /// Resolve a report's values to cells, interning any that are new.
    pub fn cells_for_report(
        &self,
        vals: &HashMap<String, Vec<String>>,
        out: &mut Vec<u32>,
    ) -> Result<(), String> {
        // Slots are permanent. Recycling them can invalidate an already-resolved
        // report, query or snapshot even when no current device uses the slot.
        let mut interner = self.interner.write().unwrap();
        self.schema.cells_for(
            vals,
            out,
            &mut Interning {
                schema: &self.schema,
                interner: &mut interner,
            },
        )
    }

    /// How many distinct values each dynamic dimension has seen.
    pub fn interned(&self) -> Vec<usize> {
        let interner = self.interner.read().unwrap();
        (0..self.schema.dims.len())
            .map(|d| interner.len(d))
            .collect()
    }

    pub fn upsert(&self, reports: &[Report<'_>]) -> Result<usize, String> {
        let cells = self.schema.cells;
        let cap = self.config.max_props_bytes;
        for r in reports {
            if !r.lng.is_finite() || !r.lat.is_finite() {
                return Err(format!("device {:?} sent a non-finite coordinate", r.id));
            }
            if let Some(cs) = r.cells {
                if cs.len() > self.schema.max_cells_per_device {
                    return Err(format!(
                        "device {:?} lands in {} filter cells, over the {} this collection allows",
                        r.id,
                        cs.len(),
                        self.schema.max_cells_per_device
                    ));
                }
                for &c in cs {
                    if c as usize >= cells {
                        return Err(format!(
                            "device {:?} has filter cell {c} but this collection has {cells}",
                            r.id
                        ));
                    }
                }
            }
            if let Some(p) = r.props {
                let raw = p.get();
                // GeoJSON properties is an object. serde already proved the text is
                // valid JSON, so the first character settles the type without a parse.
                if !raw.trim_start().starts_with('{') {
                    return Err(format!(
                        "device {:?}: props must be a JSON object, got {}",
                        r.id,
                        raw.chars().take(20).collect::<String>()
                    ));
                }
                if cap == 0 {
                    return Err(format!(
                        "device {:?} sent props but this collection has max_props_bytes = 0",
                        r.id
                    ));
                }
                if raw.len() > cap {
                    return Err(format!(
                        "device {:?}: props are {} bytes, the limit is {cap}",
                        r.id,
                        raw.len()
                    ));
                }
            }
        }
        // Extracted out here: parsing JSON while holding the write lock would
        // stall every reporter for the duration of the batch.
        let names: Vec<&str> = self.config.text.iter().map(|s| s.as_str()).collect();
        let mut text: Vec<Vec<Option<String>>> = Vec::new();
        if !names.is_empty() {
            text.reserve(reports.len());
            for r in reports {
                match r.props {
                    Some(p) => {
                        let mut got = crate::geojson::peek_text(p.get(), &names).map_err(|e| {
                            format!("device {:?}: properties are unreadable: {e}", r.id)
                        })?;
                        // lowercased once here rather than per device per query
                        for t in got.iter_mut().flatten() {
                            *t = t.to_lowercase();
                        }
                        text.push(got);
                    }
                    None => text.push(Vec::new()),
                }
            }
        }

        let now = now_ms();
        let mut accepted = 0usize;
        let mut stale = 0usize;
        let mut st = self.state.write().unwrap();
        for (i, r) in reports.iter().enumerate() {
            let n = st.ids.intern(r.id);
            if let Some(current) = st.ids.last_update[n as usize] {
                // Once versioned, an unversioned or equal-version report must
                // not roll the record back. Equal versions are idempotent retries.
                if r.updated_at_ms.map_or(true, |incoming| incoming <= current) {
                    stale += 1;
                    continue;
                }
            }
            let was_live = st.ids.last_seen[n as usize] != u64::MAX;
            let projected = netcluster::project(r.lng, r.lat);
            let target_cells = match r.cells {
                Some(cells) => cells.to_vec(),
                None if was_live => st.ids.cells[n as usize].clone(),
                None => self.default_cells.clone(),
            };
            if was_live {
                // `r.cells` of None leaves the device's filter values alone; Some
                // re-files it. Before this the values were frozen at first insert,
                // so a vehicle's status could never change -- and a status change
                // does not move the vehicle, so nothing else would notice.
                st.index
                    .move_to_projected_cells(n, projected.0, projected.1, r.cells);
            } else {
                // A new device with no values named still has to land somewhere,
                // and that somewhere is value 0 in every dimension.
                st.index
                    .insert_projected_cells(n, projected.0, projected.1, &target_cells);
            }
            st.ids.positions[n as usize] = Some(projected);
            if !was_live || r.cells.is_some() {
                st.ids.cells[n as usize] = target_cells;
            }
            st.ids.last_seen[n as usize] = now;
            // The index is a materialised view. If its direct position lookup
            // disagrees with the just-accepted report, repair it from the
            // independent position/cell records rather than exposing a no-op.
            if st.index.position(n) != Some(projected)
                || st.index.cells_of(n) != Some(st.ids.cells[n as usize].as_slice())
            {
                Self::rebuild_index(&mut st);
                self.repairs.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[repair] collection {} rebuilt after position mismatch for device {:?}",
                    self.name, r.id
                );
            }
            if let Some(version) = r.updated_at_ms {
                st.ids.last_update[n as usize] = Some(version);
            }
            if let Some(p) = r.props {
                // Keep the already-validated raw text without parsing under the lock.
                st.ids.props[n as usize] = Some(Arc::new(p.to_owned()));
                // Searchable fields follow the properties they came from: a report
                // that replaces `props` replaces these, and one that omits it
                // leaves both alone.
                if !names.is_empty() {
                    let base = n as usize * st.ids.fields;
                    for (f, v) in text[i].iter().enumerate() {
                        st.ids.text[base + f] = v.as_deref().map(Box::from);
                    }
                }
            }
            accepted += 1;
        }
        self.live_devices.store(st.index.len(), Ordering::Relaxed);
        self.ingested.fetch_add(accepted as u64, Ordering::Relaxed);
        self.stale_reports
            .fetch_add(stale as u64, Ordering::Relaxed);
        Ok(accepted)
    }

    /// Rebuild the derived tree from the independent per-device records. This
    /// is intentionally cold-path recovery, not normal update behavior.
    fn rebuild_index(st: &mut Inner) {
        let mut fresh = NetCluster::new(st.index.options());
        let stats = st.index.stats;
        for (n, &seen) in st.ids.last_seen.iter().enumerate() {
            if seen == u64::MAX {
                continue;
            }
            if let Some((x, y)) = st.ids.positions[n] {
                fresh.insert_projected_cells(n as u64, x, y, &st.ids.cells[n]);
            }
        }
        fresh.stats = stats;
        st.index = fresh;
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut st = self.state.write().unwrap();
        let Some(&n) = st.ids.to_num.get(id) else {
            return false;
        };
        let gone = st.index.remove(n);
        if gone {
            st.ids.last_seen[n as usize] = u64::MAX;
            st.ids.last_update[n as usize] = None;
            st.ids.positions[n as usize] = None;
            st.ids.cells[n as usize].clear();
            // Interning is permanent, so without this a device that comes back
            // silently inherits the properties it had in a previous life.
            st.ids.props[n as usize] = None;
            let base = n as usize * st.ids.fields;
            for f in 0..st.ids.fields {
                st.ids.text[base + f] = None;
            }
        }
        self.live_devices.store(st.index.len(), Ordering::Relaxed);
        gone
    }

    /// Is a device with this id currently in the index?
    ///
    /// A read lock and a hash lookup. Distinct from "have we ever seen this id":
    /// interning is permanent, so a removed device keeps its number, and only the
    /// index is asked here.
    pub fn contains(&self, id: &str) -> bool {
        let st = self.state.read().unwrap();
        match st.ids.to_num.get(id) {
            Some(&n) => st.index.contains(n),
            None => false,
        }
    }

    /// Everything known about one device, or `None` if it is not registered.
    pub fn device(&self, id: &str) -> Option<DeviceInfo> {
        let st = self.state.read().unwrap();
        let &n = st.ids.to_num.get(id)?;
        if !st.index.contains(n) {
            return None;
        }
        let (x, y) = st.ids.positions[n as usize]?;
        let (lng, lat) = netcluster::unproject(x as f64, y as f64);
        let cat_index = st.index.category_of(n)?;
        let last_seen_ms = st.ids.last_seen[n as usize];
        Some(DeviceInfo {
            id: id.to_string(),
            lng,
            lat,
            cat: self.config.categories.get(cat_index as usize).cloned(),
            cat_index,
            last_seen_ms,
            age_ms: now_ms().saturating_sub(last_seen_ms),
            updated_at_ms: st.ids.last_update[n as usize],
            props: st.ids.props.get(n as usize).cloned().flatten(),
        })
    }

    pub fn len(&self) -> usize {
        self.live_devices.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clusters(&self, bbox: [f64; 4], zoom: f64, cat: i32) -> Vec<OutFeature> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        // The index reads any negative cell as "no filter", so NO_MATCH has to be
        // caught here. Letting it through would answer "which vehicles belong to
        // this client nobody has heard of" with the entire fleet.
        if cat == NO_MATCH {
            return Vec::new();
        }
        let st = self.state.read().unwrap();
        st.index
            .get_clusters(bbox, zoom, cat)
            .into_iter()
            .map(|f| Self::out(&st.ids, f))
            .collect()
    }

    fn out(ids: &IdMap, f: Feature) -> OutFeature {
        match f {
            Feature::Point { id, lng, lat } => OutFeature {
                lng,
                lat,
                count: 1,
                device: Some(ids.name(id).to_string()),
                cluster_id: None,
                // An Arc clone: a refcount bump, not a copy of the text.
                props: ids.props.get(id as usize).cloned().flatten(),
            },
            Feature::Cluster {
                cluster_id,
                count,
                lng,
                lat,
            } => OutFeature {
                lng,
                lat,
                count,
                device: None,
                cluster_id: Some(cluster_id),
                props: None,
            },
        }
    }

    pub fn tile(&self, z: i32, x: i64, y: i64, cat: i32) -> Vec<OutTileFeature> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        if cat == NO_MATCH {
            return Vec::new(); // see clusters()
        }
        let st = self.state.read().unwrap();
        st.index
            .get_tile(z, x, y, cat)
            .into_iter()
            .map(|t| OutTileFeature {
                x: t.x,
                y: t.y,
                count: t.count,
                id: t.id,
                device: if t.is_cluster {
                    None
                } else {
                    Some(st.ids.name(t.id).to_string())
                },
                props: if t.is_cluster {
                    None
                } else {
                    st.ids.props.get(t.id as usize).cloned().flatten()
                },
            })
            .collect()
    }

    /// Which cluster is this device drawn as, at this zoom?
    /// Clusters matching a text search, which is a scan rather than a lookup.
    ///
    /// A substring has nothing to keep a running count of, so this cannot read a
    /// precomputed aggregate the way a declared filter does. Instead every live
    /// device is tested and the survivors are grouped by the marker they would be
    /// drawn as -- `representative_slot` answers that from the tree, so the
    /// grouping is exactly the one an unfiltered query produces, restricted to
    /// the matches. Counts and centroids are therefore exact.
    ///
    /// The cost is `O(devices)`, not `O(markers)`. That is the whole trade and it
    /// is why this is a separate entry point rather than another argument to
    /// `clusters`: nobody should reach it by accident.
    pub fn search(
        &self,
        bbox: [f64; 4],
        zoom: f64,
        cat: i32,
        preds: &[TextPred],
    ) -> Vec<OutFeature> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        if cat == NO_MATCH {
            return Vec::new();
        }
        let st = self.state.read().unwrap();
        let z = (zoom.floor() as i32).clamp(0, self.config.max_zoom as i32);
        let (x0, y0) = netcluster::project(bbox[0], bbox[3]);
        let (x1, y1) = netcluster::project(bbox[2], bbox[1]);
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));

        // marker slot -> (count, sum x, sum y, one member)
        let mut groups: HashMap<u32, (u32, i64, i64, u64)> = HashMap::new();
        let fields = st.ids.fields;
        for n in 0..st.ids.to_str.len() {
            if st.ids.last_seen[n] == u64::MAX {
                continue; // interned once, not currently live
            }
            let base = n * fields;
            if !preds
                .iter()
                .all(|p| p.test(st.ids.text[base + p.field].as_deref()))
            {
                continue;
            }
            let id = n as u64;
            if cat >= 0 {
                match st.index.cells_of(id) {
                    Some(cells) if cells.contains(&(cat as u32)) => {}
                    _ => continue,
                }
            }
            let (Some(rep), Some((x, y))) =
                (st.index.representative_slot(id, z), st.ids.positions[n])
            else {
                continue;
            };
            let e = groups.entry(rep).or_insert((0, 0, 0, id));
            e.0 += 1;
            e.1 += x as i64;
            e.2 += y as i64;
        }

        let mut out = Vec::with_capacity(groups.len());
        for (_, (count, sx, sy, member)) in groups {
            let (mx, my) = (sx / count as i64, sy / count as i64);
            // The marker is placed at the centroid of the matches, so the viewport
            // test has to be against that and not the unfiltered centre.
            if mx < x0 as i64 || mx > x1 as i64 || my < y0 as i64 || my > y1 as i64 {
                continue;
            }
            let (lng, lat) = netcluster::unproject(mx as f64, my as f64);
            out.push(if count == 1 {
                OutFeature {
                    lng,
                    lat,
                    count: 1,
                    device: Some(st.ids.to_str[member as usize].clone()),
                    cluster_id: None,
                    props: st.ids.props[member as usize].clone(),
                }
            } else {
                OutFeature {
                    lng,
                    lat,
                    count,
                    // A cluster of *matches* is not a node of the tree, so it has
                    // no id to expand: getChildren would answer about the whole
                    // cluster, including everything that did not match.
                    device: None,
                    cluster_id: None,
                    props: None,
                }
            });
        }
        out
    }

    pub fn device_cluster(&self, id: &str, zoom: i32) -> Option<OutFeature> {
        let st = self.state.read().unwrap();
        let &n = st.ids.to_num.get(id)?;
        st.index.cluster_of(n, zoom).map(|f| Self::out(&st.ids, f))
    }

    pub fn children(&self, cluster_id: u64) -> Result<Vec<OutFeature>, String> {
        let st = self.state.read().unwrap();
        st.index
            .get_children(cluster_id)
            .map(|v| v.into_iter().map(|f| Self::out(&st.ids, f)).collect())
            .map_err(|e| e.to_string())
    }

    pub fn expansion_zoom(&self, cluster_id: u64) -> Result<i32, String> {
        let st = self.state.read().unwrap();
        st.index
            .get_cluster_expansion_zoom(cluster_id)
            .map_err(|e| e.to_string())
    }

    pub fn leaves(
        &self,
        cluster_id: u64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<OutFeature>, String> {
        let st = self.state.read().unwrap();
        st.index
            .get_leaves(cluster_id, limit, offset)
            .map(|v| v.into_iter().map(|f| Self::out(&st.ids, f)).collect())
            .map_err(|e| e.to_string())
    }

    /// Drop devices that have not reported within the TTL.
    ///
    /// Two phases on purpose: find the victims under a read lock, then remove them
    /// in small batches. A single write lock around the whole sweep would stall
    /// every query for as long as the sweep took.
    pub fn sweep(&self) -> usize {
        if self.config.ttl_seconds == 0 {
            return 0;
        }
        let cutoff = now_ms().saturating_sub(self.config.ttl_seconds * 1000);
        let victims: Vec<u64> = {
            let st = self.state.read().unwrap();
            st.ids
                .last_seen
                .iter()
                .enumerate()
                .filter(|(_, &t)| t != u64::MAX && t < cutoff)
                .map(|(i, _)| i as u64)
                .collect()
        };
        if victims.is_empty() {
            return 0;
        }
        let mut dropped = 0;
        for chunk in victims.chunks(SWEEP_CHUNK) {
            let mut st = self.state.write().unwrap();
            for &n in chunk {
                // re-check: the device may have reported since the scan
                if st.ids.last_seen[n as usize] != u64::MAX
                    && st.ids.last_seen[n as usize] < cutoff
                    && st.index.remove(n)
                {
                    st.ids.last_seen[n as usize] = u64::MAX;
                    st.ids.last_update[n as usize] = None;
                    st.ids.positions[n as usize] = None;
                    st.ids.cells[n as usize].clear();
                    st.ids.props[n as usize] = None;
                    let base = n as usize * st.ids.fields;
                    for f in 0..st.ids.fields {
                        st.ids.text[base + f] = None;
                    }
                    dropped += 1;
                }
            }
            self.live_devices.store(st.index.len(), Ordering::Relaxed);
        }
        self.expired.fetch_add(dropped as u64, Ordering::Relaxed);
        dropped
    }

    pub fn stats(&self) -> CollectionStats {
        // Keep lock ordering consistent with report cell resolution: interner
        // first, collection state second. This also makes capacity exhaustion
        // visible without ever holding the two locks in reverse order.
        let interner = self.interner.read().unwrap();
        let interned: Vec<usize> = (0..self.schema.dims.len())
            .map(|d| interner.len(d))
            .collect();
        let dimension_capacities: Vec<usize> =
            self.schema.dims.iter().map(Dimension::size).collect();
        let st = self.state.read().unwrap();
        let s = st.index.stats;
        CollectionStats {
            name: self.name.clone(),
            devices: st.index.len(),
            max_zoom: self.config.max_zoom,
            radius: self.config.radius,
            categories: self.config.categories.clone(),
            ttl_seconds: self.config.ttl_seconds,
            memory_bytes: st.index.memory_bytes(),
            grid_entries: st.index.grid_entries(),
            centers_per_level: st.index.centers_per_level(),
            ingested: self.ingested.load(Ordering::Relaxed),
            queries: self.queries.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
            stale_reports: self.stale_reports.load(Ordering::Relaxed),
            repairs: self.repairs.load(Ordering::Relaxed),
            uptime_ms: now_ms().saturating_sub(self.created_ms),
            moves_fast_pct: if s.moves > 0 {
                100.0 * s.moves_fast as f64 / s.moves as f64
            } else {
                0.0
            },
            last_snapshot_ms: self.last_snapshot_ms.load(Ordering::Relaxed),
            last_snapshot_bytes: self.last_snapshot_bytes.load(Ordering::Relaxed),
            snapshot_failures: self.snapshot_failures.load(Ordering::Relaxed),
            restored: self.restored.load(Ordering::Relaxed),
            interned,
            dimension_capacities,
            props_bytes: st
                .ids
                .props
                .iter()
                .filter_map(|p| p.as_ref().map(|v| v.get().len()))
                .sum(),
            max_props_bytes: self.config.max_props_bytes,
            text_bytes: st
                .ids
                .text
                .iter()
                .map(|t| t.as_ref().map_or(0, |v| v.len() + 16))
                .sum(),
        }
    }

    /// Run the full invariant check. Admin only: `O(N²)`.
    pub fn verify(&self) -> Result<String, String> {
        let st = self.state.read().unwrap();
        for (n, &seen) in st.ids.last_seen.iter().enumerate() {
            if seen == u64::MAX {
                continue;
            }
            if st.index.position(n as u64) != st.ids.positions[n]
                || st.index.cells_of(n as u64) != Some(st.ids.cells[n].as_slice())
            {
                return Err(format!(
                    "source/index mismatch for device {:?}",
                    st.ids.name(n as u64)
                ));
            }
        }
        st.index.verify().map(|v| format!("{v:?}"))
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn a_missing_derived_record_is_repaired_on_the_next_report() {
        let c = Collection::new(
            "test",
            Config {
                ttl_seconds: 0,
                ..Config::default()
            },
        );
        let props = RawValue::from_string(r#"{"plate":"NEW"}"#.into()).unwrap();
        let report = Report {
            id: "v",
            lng: 1.,
            lat: 1.,
            cells: None,
            props: None,
            updated_at_ms: Some(100),
        };
        c.upsert(std::slice::from_ref(&report)).unwrap();
        {
            let mut st = c.state.write().unwrap();
            assert!(st.index.remove(0)); // fault injection, keep independent source data
        }
        assert!(c.verify().is_err());
        assert_eq!(
            c.upsert(&[Report {
                lng: 20.,
                lat: 20.,
                props: Some(&props),
                updated_at_ms: Some(200),
                ..report
            }])
            .unwrap(),
            1
        );
        // The core's move operation reinserts a missing ID itself; a full
        // collection rebuild is unnecessary for this recoverable case.
        assert_eq!(c.repairs.load(Ordering::Relaxed), 0);
        assert!(c.verify().is_ok());
        let hits = c.clusters([-180., -85., 180., 85.], 20., -1);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].lng - 20.).abs() < 1e-6);
        assert!(hits[0].props.as_ref().unwrap().get().contains("NEW"));
    }

    #[test]
    fn health_count_does_not_acquire_the_index_lock() {
        let c = Collection::new("test", Config::default());
        let _held = c.state.write().unwrap();
        assert_eq!(c.len(), 0);
        assert!(c.is_empty());
    }
}
