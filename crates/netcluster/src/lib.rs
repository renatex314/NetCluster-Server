//! Fully dynamic hierarchical geospatial clustering.
//!
//! Like [supercluster], except you can move a point without rebuilding the
//! index. Supercluster builds a static KD-tree per zoom level; it is excellent,
//! and it is immutable by design. If your points move -- vehicles, deliveries,
//! people -- you either rebuild the whole index on a timer or you fall back to
//! fixed-grid bucketing and accept the boundary artifacts.
//!
//! This crate maintains a **hierarchy of nets** (a cover tree over Web Mercator)
//! whose invariants are repaired locally on every update. Nothing is ever
//! recomputed globally, and there is no periodic rebuild.
//!
//! ```text
//! supercluster:  one device moved  ->  reload 500k points  ->  ~850 ms
//! netcluster:    one device moved  ->  ~2 us
//! ```
//!
//! # Cost
//!
//! | operation | cost |
//! |---|---|
//! | insert / move / remove | `O(log Δ)`, independent of `N` |
//! | viewport query | `O(K)` for `K` clusters returned |
//! | filtered viewport query | the same -- filtering is precomputed, not scanned |
//!
//! `Δ` is the aspect ratio of the point set (the ratio of the largest to the
//! smallest inter-point distance), which for geographic data is a small constant
//! times the number of zoom levels.
//!
//! # Concurrency
//!
//! Every query method takes `&self` and every mutation takes `&mut self`, so an
//! `RwLock<NetCluster>` gives you concurrent readers against a single writer with
//! no further machinery.
//!
//! # Example
//!
//! ```
//! use netcluster::{NetCluster, Options, Feature};
//!
//! let mut nc = NetCluster::new(Options { max_zoom: 16, ..Default::default() });
//!
//! for i in 0..1000u64 {
//!     let jitter = (i as f64) * 0.0001;
//!     nc.insert(i, -46.63 + jitter, -23.55 + jitter);
//! }
//!
//! let clusters = nc.get_clusters([-47.0, -24.0, -46.0, -23.0], 10.0, -1);
//! assert!(clusters.len() < 1000, "points near each other should merge");
//!
//! // Every point is accounted for exactly once, at every zoom.
//! let total: u32 = clusters.iter().map(|f| f.count()).sum();
//! assert_eq!(total, 1000);
//! ```
//!
//! [supercluster]: https://github.com/mapbox/supercluster

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod cellhash;
mod feature;
mod index;
mod project;

pub use cellhash::CellHash;
pub use feature::{ClusterIdError, Feature, TileFeature};
pub use index::{NetCluster, Options, Slot, Stats, Verification, NONE};
pub use project::{project, unproject, PREC, PREC_BITS, PREC_I};
