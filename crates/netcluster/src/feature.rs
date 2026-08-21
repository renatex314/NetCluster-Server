//! What a query returns.

use std::fmt;

/// One thing to draw on a map: either a single point or a cluster standing in
/// for several.
///
/// The split is an enum rather than a struct with a `count` field because the
/// two cases carry different identities -- a point is identified by *your* id,
/// a cluster by an opaque handle into the tree that is only meaningful until the
/// next mutation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Feature {
    Point {
        /// The external id you inserted the point under.
        id: u64,
        lng: f64,
        lat: f64,
    },
    Cluster {
        /// Opaque handle for [`NetCluster::get_children`], [`NetCluster::get_leaves`]
        /// and [`NetCluster::get_cluster_expansion_zoom`].
        ///
        /// It encodes a slot and a zoom level, and it is invalidated by any
        /// mutation that frees that slot. Do not persist it, and do not treat it
        /// as stable across a query.
        ///
        /// [`NetCluster::get_children`]: crate::NetCluster::get_children
        /// [`NetCluster::get_leaves`]: crate::NetCluster::get_leaves
        /// [`NetCluster::get_cluster_expansion_zoom`]: crate::NetCluster::get_cluster_expansion_zoom
        cluster_id: u64,
        /// How many points this cluster stands for. Always `>= 2`.
        count: u32,
        /// Centroid, exactly the arithmetic mean of the member coordinates.
        lng: f64,
        lat: f64,
    },
}

impl Feature {
    #[inline]
    pub fn lng(&self) -> f64 {
        match *self {
            Feature::Point { lng, .. } | Feature::Cluster { lng, .. } => lng,
        }
    }

    #[inline]
    pub fn lat(&self) -> f64 {
        match *self {
            Feature::Point { lat, .. } | Feature::Cluster { lat, .. } => lat,
        }
    }

    /// 1 for a single point, otherwise the cluster's member count.
    #[inline]
    pub fn count(&self) -> u32 {
        match *self {
            Feature::Point { .. } => 1,
            Feature::Cluster { count, .. } => count,
        }
    }

    #[inline]
    pub fn is_cluster(&self) -> bool {
        matches!(self, Feature::Cluster { .. })
    }

    /// `"1.2k"`, `"34k"` -- the abbreviation convention supercluster uses for
    /// marker labels.
    pub fn count_abbreviated(&self) -> String {
        abbrev(self.count())
    }
}

pub(crate) fn abbrev(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else if n >= 1000 {
        let tenths = (n as f64 / 100.0).round() / 10.0;
        format!("{tenths}k")
    } else {
        n.to_string()
    }
}

/// A point positioned inside a vector tile, in tile-extent coordinates.
///
/// `x` and `y` may fall slightly outside `0..extent`: the query pads the tile by
/// the cluster radius so that a marker straddling the seam is emitted by both
/// neighbouring tiles instead of being clipped in half.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileFeature {
    pub x: i32,
    pub y: i32,
    pub count: u32,
    /// External id when `count == 1`, otherwise the cluster handle.
    pub id: u64,
    pub is_cluster: bool,
}

/// The only thing that can go wrong reading a cluster handle back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterIdError {
    pub cluster_id: u64,
}

impl fmt::Display for ClusterIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "netcluster: cluster id {} does not refer to a live cluster. Cluster ids come from \
             Feature::Cluster returned by get_clusters(), and are invalidated by mutations. \
             A device id is not a cluster id -- use representative(id, zoom) for that.",
            self.cluster_id
        )
    }
}

impl std::error::Error for ClusterIdError {}

#[cfg(test)]
mod tests {
    use super::abbrev;

    #[test]
    fn abbreviation_matches_the_supercluster_convention() {
        assert_eq!(abbrev(7), "7");
        assert_eq!(abbrev(999), "999");
        assert_eq!(abbrev(1000), "1k");
        assert_eq!(abbrev(1240), "1.2k");
        assert_eq!(abbrev(9999), "10k");
        assert_eq!(abbrev(10_000), "10k");
        assert_eq!(abbrev(123_400), "123k");
    }
}
