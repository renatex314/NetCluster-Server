//! Reading GeoJSON on the ingest path.
//!
//! `/clusters` has always *emitted* GeoJSON. This is the other direction: a
//! `FeatureCollection` posted to `/positions` is accepted alongside the compact
//! `{id, lng, lat}` form.
//!
//! Everything here is hand-written rather than derived, for the same reason
//! `PositionsBody` is: `properties` must survive as `RawValue`, the original
//! bytes, so that a read can hand it straight to the serialiser without ever
//! having parsed it. A derived `#[serde(untagged)]` enum buffers into an
//! intermediate representation and throws the original text away, which is
//! exactly what `RawValue` needs.
//!
//! Unknown keys are ignored rather than rejected -- the opposite of `ReportBody`,
//! on purpose. RFC 7946 section 6.1 explicitly allows foreign members on a
//! Feature, so real files carry them and refusing them would reject valid
//! GeoJSON. The compact form has no such licence, so a stray key there is still
//! a mistake worth failing on.

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::fmt;

/// A GeoJSON id, normalised to the string the server keys devices by.
///
/// GeoJSON allows a string or a number. A number becomes its decimal form, so
/// `7` and `"7"` are the same device -- which is what anyone round-tripping a
/// file through a JSON encoder that stringifies ids would expect.
pub struct IdString(pub String);

impl<'de> Deserialize<'de> for IdString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = IdString;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or integer id")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<IdString, E> {
                Ok(IdString(v.to_owned()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<IdString, E> {
                Ok(IdString(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<IdString, E> {
                Ok(IdString(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<IdString, E> {
                // A fractional id is a bug, not a device. Saying so beats keying
                // a fleet by "1.7999999999999998".
                if v.fract() == 0.0 && v.abs() < 9e15 {
                    Ok(IdString((v as i64).to_string()))
                } else {
                    Err(E::custom(format!("id {v} is not a whole number")))
                }
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<IdString, E> {
                Err(E::custom(format!("id is {v}, expected a string or a number")))
            }
            fn visit_unit<E: de::Error>(self) -> Result<IdString, E> {
                Err(E::custom("id is null, expected a string or a number"))
            }
        }
        d.deserialize_any(V)
    }
}

/// A category as it may appear in `properties`: the index itself, or the name
/// declared on the collection.
pub enum CatVal {
    Num(u32),
    Name(String),
}

impl<'de> Deserialize<'de> for CatVal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = CatVal;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a category index or name")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<CatVal, E> {
                Ok(CatVal::Name(v.to_owned()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<CatVal, E> {
                Ok(CatVal::Num(v as u32))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<CatVal, E> {
                if v < 0 {
                    return Err(E::custom(format!("category {v} is negative")));
                }
                Ok(CatVal::Num(v as u32))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<CatVal, E> {
                if v.fract() == 0.0 && v >= 0.0 {
                    Ok(CatVal::Num(v as u32))
                } else {
                    Err(E::custom(format!("category {v} is not a whole number")))
                }
            }
        }
        d.deserialize_any(V)
    }
}

/// A Point geometry. `null` geometry is legal GeoJSON and useless here, so it is
/// represented by the absence of this and rejected with a message that says so.
#[derive(Debug)]
pub struct PointGeom {
    pub lng: f64,
    pub lat: f64,
}

/// One element of a coordinates array, classified without being materialised.
///
/// A number, or something that is not a number -- an inner array, for a Polygon
/// or a LineString. Needed because `coordinates` may be read before `type`, and
/// failing there would report "invalid type: sequence, expected f64" for a
/// Polygon when what the sender needs to be told is that it is a Polygon.
enum Elem {
    Num(f64),
    NotANumber,
}

impl<'de> Deserialize<'de> for Elem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Elem;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a coordinate")
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Elem, E> {
                Ok(Elem::Num(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Elem, E> {
                Ok(Elem::Num(v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Elem, E> {
                Ok(Elem::Num(v as f64))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Elem, E> {
                Ok(Elem::NotANumber)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Elem, E> {
                Ok(Elem::NotANumber)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Elem, E> {
                Ok(Elem::NotANumber)
            }
            fn visit_none<E: de::Error>(self) -> Result<Elem, E> {
                Ok(Elem::NotANumber)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Elem, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Elem::NotANumber)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Elem, A::Error> {
                while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Elem::NotANumber)
            }
        }
        d.deserialize_any(V)
    }
}

/// `[lng, lat]`, or `[lng, lat, altitude]` -- the third element is elevation,
/// which GeoJSON allows and clustering has no use for.
///
/// `None` means the array was not a flat pair of numbers. It is carried rather
/// than raised so that `type` can be judged first, whichever order the two keys
/// arrived in.
struct Coords(Option<(f64, f64)>);

impl<'de> Deserialize<'de> for Coords {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Coords;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("[longitude, latitude]")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Coords, A::Error> {
                let first: Option<Elem> = a.next_element()?;
                let second: Option<Elem> = a.next_element()?;
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Coords(match (first, second) {
                    (Some(Elem::Num(lng)), Some(Elem::Num(lat))) => Some((lng, lat)),
                    _ => None,
                }))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Coords, A::Error> {
                while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Coords(None))
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for PointGeom {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = PointGeom;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a GeoJSON Point geometry")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<PointGeom, A::Error> {
                let mut ty: Option<String> = None;
                let mut co: Option<Coords> = None;
                // Both orders occur in the wild, so neither key may be validated
                // until the map is exhausted.
                while let Some(k) = m.next_key::<&str>()? {
                    match k {
                        "type" => ty = Some(m.next_value()?),
                        "coordinates" => co = Some(m.next_value()?),
                        _ => {
                            m.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                match ty.as_deref() {
                    Some("Point") => {}
                    // Rejected rather than quietly reduced to a centroid: a
                    // silently mis-placed polygon is a wrong map with no error.
                    Some(other) => {
                        return Err(de::Error::custom(format!(
                            "geometry is a {other}, expected a Point -- reduce areas and lines to \
                             a representative point before sending them"
                        )))
                    }
                    None => return Err(de::Error::custom("geometry has no type")),
                }
                let (lng, lat) = co
                    .ok_or_else(|| de::Error::custom("Point geometry has no coordinates"))?
                    .0
                    .ok_or_else(|| {
                        de::Error::custom(
                            "Point coordinates are not a [longitude, latitude] pair of numbers",
                        )
                    })?;
                Ok(PointGeom { lng, lat })
            }
        }
        d.deserialize_map(V)
    }
}

/// One Feature, read but not adopted: the wrapper is dropped and only these
/// three values go on.
pub struct GeoFeature {
    pub id: Option<String>,
    /// `None` when the Feature carried a null geometry.
    pub geom: Option<PointGeom>,
    pub props: Option<Box<RawValue>>,
}

impl<'de> Deserialize<'de> for GeoFeature {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = GeoFeature;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a GeoJSON Feature")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<GeoFeature, A::Error> {
                let mut id = None;
                let mut geom = None;
                let mut props = None;
                let mut saw_geometry = false;
                let mut ty: Option<String> = None;
                while let Some(k) = m.next_key::<&str>()? {
                    match k {
                        "type" => ty = Some(m.next_value()?),
                        "id" => id = m.next_value::<Option<IdString>>()?.map(|v| v.0),
                        "geometry" => {
                            saw_geometry = true;
                            geom = m.next_value::<Option<PointGeom>>()?;
                        }
                        "properties" => props = m.next_value::<Option<Box<RawValue>>>()?,
                        _ => {
                            m.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                if let Some(t) = ty.as_deref() {
                    if t != "Feature" {
                        return Err(de::Error::custom(if t == "FeatureCollection" {
                            "a FeatureCollection nested inside features".to_string()
                        } else {
                            format!("type is {t:?}, expected \"Feature\"")
                        }));
                    }
                }
                if !saw_geometry {
                    return Err(de::Error::custom("Feature has no geometry"));
                }
                // `properties: null` is GeoJSON for "none", which must mean
                // "leave whatever is stored alone" -- the same as omitting
                // `props` in the compact form. An explicit {} still clears.
                if props.as_ref().is_some_and(|p| p.get().trim() == "null") {
                    props = None;
                }
                Ok(GeoFeature { id, geom, props })
            }
        }
        d.deserialize_map(V)
    }
}

/// Which of the properties we are looking for a key turned out to be.
enum Which {
    Id,
    Cat(usize),
    Other,
}

/// Classifies a key without allocating, and without caring whether serde_json
/// could borrow it -- an escaped key arrives through the same `visit_str`.
struct KeySeed<'k> {
    id_key: Option<&'k str>,
    cat_keys: &'k [&'k str],
}

impl<'de, 'k> DeserializeSeed<'de> for KeySeed<'k> {
    type Value = Which;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Which, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'k> Visitor<'_> for KeySeed<'k> {
    type Value = Which;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a property name")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Which, E> {
        if self.id_key == Some(v) {
            return Ok(Which::Id);
        }
        match self.cat_keys.iter().position(|c| *c == v) {
            Some(i) => Ok(Which::Cat(i)),
            None => Ok(Which::Other),
        }
    }
}

/// Pull at most an id and a category out of a properties object.
///
/// One skip-scan over text serde_json has already proved is valid JSON.
/// Everything not asked for is stepped over as `IgnoredAny` rather than
/// materialised, so this allocates only for the values it actually finds --
/// which is what lets `properties` stay stored as untouched bytes.
///
/// `cat_keys` is ordered by preference: the earliest one present wins, so a
/// collection can accept `cat` while still reading `category` from files that
/// use the longer name.
pub fn peek_props(
    raw: &str,
    id_key: Option<&str>,
    cat_keys: &[&str],
) -> Result<(Option<String>, Option<CatVal>), serde_json::Error> {
    struct Peek<'k> {
        id_key: Option<&'k str>,
        cat_keys: &'k [&'k str],
    }
    impl<'de, 'k> DeserializeSeed<'de> for Peek<'k> {
        type Value = (Option<String>, Option<CatVal>);
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de, 'k> Visitor<'de> for Peek<'k> {
        type Value = (Option<String>, Option<CatVal>);
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a properties object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
            let mut id = None;
            let mut cat = None;
            let mut rank = usize::MAX;
            while let Some(which) = m.next_key_seed(KeySeed {
                id_key: self.id_key,
                cat_keys: self.cat_keys,
            })? {
                match which {
                    Which::Id => id = m.next_value::<Option<IdString>>()?.map(|v| v.0),
                    Which::Cat(i) if i < rank => {
                        if let Some(v) = m.next_value::<Option<CatVal>>()? {
                            cat = Some(v);
                            rank = i;
                        }
                    }
                    _ => {
                        m.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok((id, cat))
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw);
    let out = Peek { id_key, cat_keys }.deserialize(&mut d)?;
    d.end()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peek(raw: &str, id: Option<&str>, cats: &[&str]) -> (Option<String>, Option<u32>, Option<String>) {
        let (i, c) = peek_props(raw, id, cats).unwrap();
        let (n, name) = match c {
            Some(CatVal::Num(n)) => (Some(n), None),
            Some(CatVal::Name(s)) => (None, Some(s)),
            None => (None, None),
        };
        (i, n, name)
    }

    #[test]
    fn pulls_out_only_what_it_was_asked_for() {
        let raw = r#"{"plate":"ABC","cat":2,"route":{"stops":[1,2,3]},"id":"v7"}"#;
        assert_eq!(peek(raw, Some("id"), &["cat"]), (Some("v7".into()), Some(2), None));
        assert_eq!(peek(raw, None, &["cat"]), (None, Some(2), None));
        assert_eq!(peek(raw, Some("plate"), &[]), (Some("ABC".into()), None, None));
        assert_eq!(peek(raw, None, &[]), (None, None, None));
    }

    #[test]
    fn the_earliest_listed_category_key_wins() {
        // Both spellings present: `cat` is listed first, so it is the one used --
        // and the loser must still be stepped over rather than left unread.
        let raw = r#"{"category":"enroute","cat":1}"#;
        assert_eq!(peek(raw, None, &["cat", "category"]), (None, Some(1), None));
        assert_eq!(peek(raw, None, &["category", "cat"]), (None, None, Some("enroute".into())));
    }

    #[test]
    fn an_escaped_key_still_matches() {
        // serde_json cannot borrow a key containing an escape, so a classifier
        // that only accepted borrowed strings would fail on valid JSON.
        let raw = r#"{"cat":3}"#;
        assert_eq!(peek(raw, None, &["cat"]), (None, Some(3), None));
    }

    #[test]
    fn a_numeric_id_becomes_its_decimal_form() {
        assert_eq!(peek(r#"{"id":7}"#, Some("id"), &[]).0, Some("7".into()));
        assert_eq!(peek(r#"{"id":7.0}"#, Some("id"), &[]).0, Some("7".into()));
        assert!(peek_props(r#"{"id":7.5}"#, Some("id"), &[]).is_err());
        // present but null is the same as absent
        assert_eq!(peek(r#"{"id":null}"#, Some("id"), &[]).0, None);
    }

    #[test]
    fn nested_values_are_skipped_not_parsed() {
        let raw = r#"{"a":{"b":{"c":[1,{"d":null},true]}},"cat":0,"e":[[[]]]}"#;
        assert_eq!(peek(raw, None, &["cat"]), (None, Some(0), None));
    }

    #[test]
    fn trailing_junk_is_rejected() {
        // `d.end()` matters: without it a truncated or double-encoded blob would
        // read as valid and quietly lose whatever came after.
        assert!(peek_props(r#"{"cat":1} extra"#, None, &["cat"]).is_err());
    }

    #[test]
    fn a_polygon_is_named_in_the_error_whichever_order_the_keys_arrive_in() {
        for body in [
            r#"{"type":"Polygon","coordinates":[[[0,0],[1,1],[1,0],[0,0]]]}"#,
            r#"{"coordinates":[[[0,0],[1,1],[1,0],[0,0]]],"type":"Polygon"}"#,
        ] {
            let e = serde_json::from_str::<PointGeom>(body).unwrap_err().to_string();
            assert!(e.contains("Polygon"), "{e}");
            assert!(e.contains("representative point"), "{e}");
        }
    }

    #[test]
    fn altitude_is_read_and_dropped() {
        let g: PointGeom =
            serde_json::from_str(r#"{"type":"Point","coordinates":[10,20,3000]}"#).unwrap();
        assert_eq!((g.lng, g.lat), (10.0, 20.0));
    }

    #[test]
    fn a_feature_keeps_its_properties_as_untouched_bytes() {
        let f: GeoFeature = serde_json::from_str(
            r#"{"type":"Feature","id":"v1","properties":{"a":1.0000000000000002,"b":"x"},
                "geometry":{"type":"Point","coordinates":[1,2]}}"#,
        )
        .unwrap();
        assert_eq!(f.id.as_deref(), Some("v1"));
        // Not re-serialised from a parsed value: the original text is what is
        // stored, so a number that cannot survive a round trip through f64
        // formatting is handed back exactly as it arrived.
        assert!(f.props.unwrap().get().contains("1.0000000000000002"));
    }

    #[test]
    fn null_properties_mean_leave_them_alone() {
        let f: GeoFeature = serde_json::from_str(
            r#"{"type":"Feature","id":"v1","properties":null,
                "geometry":{"type":"Point","coordinates":[1,2]}}"#,
        )
        .unwrap();
        assert!(f.props.is_none());
    }
}
