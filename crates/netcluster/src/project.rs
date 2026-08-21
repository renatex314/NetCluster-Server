//! Web-Mercator projection into fixed-point world units.
//!
//! The index never stores floating-point coordinates. A longitude/latitude pair
//! is projected once, on the way in, to a pair of integers in `[0, 2^30]`, and
//! everything after that -- distances, cell indices, subtree sums -- is exact
//! integer arithmetic. That is what lets an index run for months without the
//! centroid of a cluster drifting.

/// Bits of fixed-point resolution. The unit square of the Web-Mercator world
/// maps onto `[0, 2^30]`, i.e. about 3.7 cm at the equator.
pub const PREC_BITS: u32 = 30;

/// `2^PREC_BITS` as a float, for the many places that divide by it.
pub const PREC: f64 = (1u64 << PREC_BITS) as f64;

/// `2^PREC_BITS` as an integer.
pub const PREC_I: i64 = 1i64 << PREC_BITS;

/// Longitude/latitude (degrees) to fixed-point world coordinates.
///
/// Latitudes beyond the Mercator limit and longitudes outside `[-180, 180)` are
/// clamped rather than rejected: a GPS fix that lands slightly out of range is a
/// data-quality problem, not a reason to drop the report.
#[inline]
pub fn project(lng: f64, lat: f64) -> (i32, i32) {
    let mut x = (lng + 180.0) / 360.0;
    if x < 0.0 {
        x = 0.0;
    } else if x >= 1.0 {
        x = 0.9999999;
    }
    let s = (lat * std::f64::consts::PI / 180.0).sin();
    let mut y = 0.5 - 0.25 * ((1.0 + s) / (1.0 - s)).ln() / std::f64::consts::PI;
    if y < 0.0 {
        y = 0.0;
    } else if y >= 1.0 {
        y = 0.9999999;
    }
    ((x * PREC).round() as i32, (y * PREC).round() as i32)
}

/// Fixed-point world coordinates back to longitude/latitude (degrees).
///
/// Takes floats because cluster centroids are sums divided by counts, which are
/// generally not integers.
#[inline]
pub fn unproject(x: f64, y: f64) -> (f64, f64) {
    let lng = x / PREC * 360.0 - 180.0;
    let y2 = y / PREC;
    let lat = 360.0 * ((0.5 - y2) * 2.0 * std::f64::consts::PI).exp().atan() / std::f64::consts::PI
        - 90.0;
    (lng, lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_within_a_fixed_point_unit() {
        for &(lng, lat) in &[
            (0.0, 0.0),
            (-46.633, -23.55),
            (139.69, 35.68),
            (-0.1276, 51.5072),
            (179.9999, -85.0),
        ] {
            let (x, y) = project(lng, lat);
            let (lng2, lat2) = unproject(x as f64, y as f64);
            assert!((lng - lng2).abs() < 1e-6, "lng {lng} -> {lng2}");
            assert!((lat - lat2).abs() < 1e-6, "lat {lat} -> {lat2}");
        }
    }

    #[test]
    fn clamps_out_of_range_input_instead_of_panicking() {
        let (x, y) = project(-400.0, 95.0);
        assert!((0..=PREC_I as i32).contains(&x));
        assert!((0..=PREC_I as i32).contains(&y));
    }
}
