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

use crate::snapshot::DeviceRecord;
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
    pub categories: Vec<String>,
    /// Largest per-device properties blob accepted, in bytes. 0 refuses properties
    /// entirely.
    ///
    /// Memory is bounded by devices times this number, so it is a real limit and
    /// not a formality: at a million devices, every kilobyte allowed here is a
    /// gigabyte you have promised to have.
    pub max_props_bytes: usize,
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
    /// Free-form properties per interned id, as raw JSON.
    ///
    /// Held behind an `Arc` so a query copies a refcount rather than the text: at
    /// max zoom a viewport can return tens of thousands of single points, and
    /// cloning each blob would dominate the query.
    props: Vec<Option<Arc<Box<RawValue>>>>,
}

impl IdMap {
    fn intern(&mut self, id: &str) -> u64 {
        if let Some(&n) = self.to_num.get(id) {
            return n;
        }
        let n = self.to_str.len() as u64;
        self.to_str.push(id.to_string());
        self.last_seen.push(u64::MAX);
        self.props.push(None);
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

pub struct Collection {
    pub name: String,
    pub config: Config,
    state: RwLock<Inner>,
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
}

/// One position report.
#[derive(Debug, Clone)]
pub struct Report<'a> {
    pub id: &'a str,
    pub lng: f64,
    pub lat: f64,
    pub cat: u32,
    /// `None` leaves whatever the device already had; `Some` replaces it.
    ///
    /// That asymmetry is the point: properties are slow-changing metadata and
    /// positions arrive many times a second, so a position report should not have
    /// to resend the number plate to avoid erasing it. To actually clear them,
    /// send an empty object.
    pub props: Option<&'a RawValue>,
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
    /// Bytes of device properties currently held. Worth watching: it is the one
    /// part of the index whose size you control from outside.
    pub props_bytes: usize,
    pub max_props_bytes: usize,
}

impl Collection {
    pub fn new(name: &str, config: Config) -> Self {
        let index = NetCluster::new(Options {
            min_zoom: 0,
            max_zoom: config.max_zoom,
            radius: config.radius,
            extent: config.extent,
            hysteresis: config.hysteresis,
            categories: config.categories.len(),
        });
        Collection {
            name: name.to_string(),
            config,
            state: RwLock::new(Inner {
                index,
                ids: IdMap::default(),
            }),
            created_ms: now_ms(),
            ingested: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            last_snapshot_ms: AtomicU64::new(0),
            last_snapshot_bytes: AtomicU64::new(0),
            snapshot_failures: AtomicU64::new(0),
            restored: AtomicU64::new(0),
        }
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
    pub fn restore(name: &str, config: Config, records: &[DeviceRecord]) -> (Self, usize) {
        let c = Collection::new(name, config);
        let cutoff = if c.config.ttl_seconds > 0 {
            now_ms().saturating_sub(c.config.ttl_seconds * 1000)
        } else {
            0
        };
        let k = c.config.categories.len();
        let mut skipped = 0usize;
        {
            let mut st = c.state.write().unwrap();
            for r in records {
                if r.last_seen_ms < cutoff {
                    skipped += 1;
                    continue;
                }
                // The config can have changed since the snapshot was written. A
                // category that no longer exists falls back to 0 rather than
                // panicking on a startup path, where a panic means the process
                // never comes back at all.
                let cat = if k > 0 && (r.cat as usize) < k {
                    r.cat
                } else {
                    0
                };
                let n = st.ids.intern(&r.id);
                st.index.insert_projected(n, r.x, r.y, cat);
                st.ids.last_seen[n as usize] = r.last_seen_ms;
                if let Some(p) = &r.props {
                    st.ids.props[n as usize] = RawValue::from_string(p.clone()).ok().map(Arc::new);
                }
            }
        }
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
            let Some((x, y)) = st.index.position(n) else {
                continue;
            };
            out.push(DeviceRecord {
                id: st.ids.to_str[n as usize].clone(),
                x,
                y,
                cat: st.index.category_of(n).unwrap_or(0),
                last_seen_ms: seen,
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
        let records = self.export();
        let meta = crate::snapshot::Meta {
            name: self.name.clone(),
            config: self.config.clone(),
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
        if let Some(i) = self.config.categories.iter().position(|c| c == sel) {
            return Ok(i as i32);
        }
        if let Ok(n) = sel.parse::<usize>() {
            if n < self.config.categories.len() {
                return Ok(n as i32);
            }
        }
        Err(format!(
            "unknown category {sel:?}; this collection has {:?}",
            self.config.categories
        ))
    }

    pub fn upsert(&self, reports: &[Report<'_>]) -> Result<usize, String> {
        let k = self.config.categories.len();
        let cap = self.config.max_props_bytes;
        for r in reports {
            if !r.lng.is_finite() || !r.lat.is_finite() {
                return Err(format!("device {:?} sent a non-finite coordinate", r.id));
            }
            if k > 0 && r.cat as usize >= k {
                return Err(format!(
                    "device {:?} has category {} but this collection has {k}",
                    r.id, r.cat
                ));
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
        let now = now_ms();
        let mut st = self.state.write().unwrap();
        for r in reports {
            let n = st.ids.intern(r.id);
            if st.index.contains(n) {
                st.index.move_to(n, r.lng, r.lat);
            } else {
                st.index.insert_with_category(n, r.lng, r.lat, r.cat);
            }
            st.ids.last_seen[n as usize] = now;
            if let Some(p) = r.props {
                // Reparsing here is what makes reads free: the blob is stored as
                // validated raw text and handed straight to the serialiser.
                st.ids.props[n as usize] =
                    RawValue::from_string(p.get().to_owned()).ok().map(Arc::new);
            }
        }
        self.ingested
            .fetch_add(reports.len() as u64, Ordering::Relaxed);
        Ok(reports.len())
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut st = self.state.write().unwrap();
        let Some(&n) = st.ids.to_num.get(id) else {
            return false;
        };
        let gone = st.index.remove(n);
        if gone {
            st.ids.last_seen[n as usize] = u64::MAX;
            // Interning is permanent, so without this a device that comes back
            // silently inherits the properties it had in a previous life.
            st.ids.props[n as usize] = None;
        }
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
        let (lng, lat) = st.index.position_of(n)?;
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
            props: st.ids.props.get(n as usize).cloned().flatten(),
        })
    }

    pub fn len(&self) -> usize {
        self.state.read().unwrap().index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.read().unwrap().index.is_empty()
    }

    pub fn clusters(&self, bbox: [f64; 4], zoom: f64, cat: i32) -> Vec<OutFeature> {
        self.queries.fetch_add(1, Ordering::Relaxed);
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
                    st.ids.props[n as usize] = None;
                    dropped += 1;
                }
            }
        }
        self.expired.fetch_add(dropped as u64, Ordering::Relaxed);
        dropped
    }

    pub fn stats(&self) -> CollectionStats {
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
            props_bytes: st
                .ids
                .props
                .iter()
                .filter_map(|p| p.as_ref().map(|v| v.get().len()))
                .sum(),
            max_props_bytes: self.config.max_props_bytes,
        }
    }

    /// Run the full invariant check. Admin only: `O(N²)`.
    pub fn verify(&self) -> Result<String, String> {
        let st = self.state.read().unwrap();
        st.index.verify().map(|v| format!("{v:?}"))
    }
}
