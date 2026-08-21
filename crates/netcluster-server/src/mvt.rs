//! A minimal Mapbox Vector Tile encoder.
//!
//! Hand-rolled rather than pulled from a crate, for one reason: the only thing
//! this server ever emits is points carrying two or three tags. A general MVT
//! library brings a protobuf compiler, a geometry model and a coordinate
//! pipeline to do what fits in a couple hundred lines here, and every one of
//! those is a dependency that has to be kept current for the lifetime of the
//! service.
//!
//! Wire format reference: <https://github.com/mapbox/vector-tile-spec/tree/master/2.1>

use std::collections::HashMap;

/// Protobuf writer. Only the two wire types the MVT schema uses.
#[derive(Default)]
struct Pbf {
    buf: Vec<u8>,
}

impl Pbf {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    fn tag(&mut self, field: u32, wire: u32) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    fn varint_field(&mut self, field: u32, v: u64) {
        self.tag(field, 0);
        self.varint(v);
    }

    fn bytes_field(&mut self, field: u32, b: &[u8]) {
        self.tag(field, 2);
        self.varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    fn packed_u32(&mut self, field: u32, vals: &[u32]) {
        let mut inner = Pbf::default();
        for &v in vals {
            inner.varint(v as u64);
        }
        self.bytes_field(field, &inner.buf);
    }
}

/// A tag value. MVT interns these per layer, which matters here: a tile of ten
/// thousand clusters holds very few distinct counts.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum Val {
    Bool(bool),
    Uint(u64),
    Str(String),
}

impl Val {
    fn encode(&self, out: &mut Pbf) {
        let mut v = Pbf::default();
        match self {
            Val::Str(s) => v.bytes_field(1, s.as_bytes()),
            Val::Uint(n) => v.varint_field(5, *n),
            Val::Bool(b) => v.varint_field(7, *b as u64),
        }
        out.bytes_field(4, &v.buf);
    }
}

/// Builds one layer of point features.
pub struct Layer {
    name: String,
    extent: u32,
    keys: Vec<String>,
    key_idx: HashMap<String, u32>,
    values: Vec<Val>,
    value_idx: HashMap<Val, u32>,
    features: Vec<u8>,
    count: usize,
}

impl Layer {
    pub fn new(name: &str, extent: u32) -> Self {
        Layer {
            name: name.to_string(),
            extent,
            keys: Vec::new(),
            key_idx: HashMap::new(),
            values: Vec::new(),
            value_idx: HashMap::new(),
            features: Vec::new(),
            count: 0,
        }
    }

    fn key(&mut self, k: &str) -> u32 {
        if let Some(&i) = self.key_idx.get(k) {
            return i;
        }
        let i = self.keys.len() as u32;
        self.keys.push(k.to_string());
        self.key_idx.insert(k.to_string(), i);
        i
    }

    fn value(&mut self, v: Val) -> u32 {
        if let Some(&i) = self.value_idx.get(&v) {
            return i;
        }
        let i = self.values.len() as u32;
        self.values.push(v.clone());
        self.value_idx.insert(v, i);
        i
    }

    /// Add a point at tile-extent coordinates. `x` and `y` may fall outside
    /// `0..extent`; renderers clip, and the overshoot is what keeps a marker on a
    /// tile seam from being drawn half in each tile.
    pub fn add_point(&mut self, id: u64, x: i32, y: i32, tags: &[(&str, Val)]) {
        let pairs: Vec<u32> = tags
            .iter()
            .flat_map(|(k, v)| {
                let ki = self.key(k);
                let vi = self.value(v.clone());
                [ki, vi]
            })
            .collect();

        let mut f = Pbf::default();
        f.varint_field(1, id);
        if !pairs.is_empty() {
            f.packed_u32(2, &pairs);
        }
        f.varint_field(3, 1); // GeomType::POINT
                              // MoveTo, one pair: (command 1) | (count 1 << 3)
        let zz = |n: i32| ((n << 1) ^ (n >> 31)) as u32;
        f.packed_u32(4, &[9, zz(x), zz(y)]);

        let mut wrapped = Pbf::default();
        wrapped.bytes_field(2, &f.buf);
        self.features.extend_from_slice(&wrapped.buf);
        self.count += 1;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn encode(self) -> Vec<u8> {
        let mut l = Pbf::default();
        l.bytes_field(1, self.name.as_bytes());
        l.buf.extend_from_slice(&self.features);
        for k in &self.keys {
            l.bytes_field(3, k.as_bytes());
        }
        for v in &self.values {
            v.encode(&mut l);
        }
        l.varint_field(5, self.extent as u64);
        l.varint_field(15, 2); // version
        l.buf
    }
}

/// Encode one or more layers into a complete tile.
pub fn encode(layers: Vec<Layer>) -> Vec<u8> {
    let mut t = Pbf::default();
    for l in layers {
        let body = l.encode();
        t.bytes_field(3, &body);
    }
    t.buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode just enough to prove the bytes are well-formed protobuf and carry
    /// what we put in. A tile that renders as nothing is indistinguishable from a
    /// tile that failed to encode, so this checks the wire form directly.
    fn read_varint(b: &[u8], i: &mut usize) -> u64 {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = b[*i];
            *i += 1;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }

    #[test]
    fn produces_a_parseable_tile() {
        let mut l = Layer::new("clusters", 4096);
        l.add_point(
            42,
            100,
            -7,
            &[
                ("cluster", Val::Bool(true)),
                ("point_count", Val::Uint(1234)),
            ],
        );
        l.add_point(7, 4000, 4000, &[("cluster", Val::Bool(false))]);
        assert_eq!(l.len(), 2);
        let tile = encode(vec![l]);
        assert!(!tile.is_empty());

        // top level: exactly one length-delimited field 3 spanning the whole buffer
        let mut i = 0;
        let key = read_varint(&tile, &mut i);
        assert_eq!(key >> 3, 3, "layers must be field 3");
        assert_eq!(key & 7, 2, "layers must be length-delimited");
        let len = read_varint(&tile, &mut i) as usize;
        assert_eq!(i + len, tile.len(), "layer length must cover the rest");

        // inside the layer: name, two features, keys, values, extent, version
        let body = &tile[i..];
        let mut j = 0;
        let (mut features, mut keys, mut values) = (0, 0, 0);
        let (mut extent, mut version) = (0, 0);
        let mut name = String::new();
        while j < body.len() {
            let k = read_varint(body, &mut j);
            match (k >> 3, k & 7) {
                (1, 2) => {
                    let n = read_varint(body, &mut j) as usize;
                    name = String::from_utf8(body[j..j + n].to_vec()).unwrap();
                    j += n;
                }
                (2, 2) => {
                    let n = read_varint(body, &mut j) as usize;
                    features += 1;
                    j += n;
                }
                (3, 2) => {
                    let n = read_varint(body, &mut j) as usize;
                    keys += 1;
                    j += n;
                }
                (4, 2) => {
                    let n = read_varint(body, &mut j) as usize;
                    values += 1;
                    j += n;
                }
                (5, 0) => extent = read_varint(body, &mut j),
                (15, 0) => version = read_varint(body, &mut j),
                other => panic!("unexpected field {other:?}"),
            }
        }
        assert_eq!(name, "clusters");
        assert_eq!(features, 2);
        assert_eq!(keys, 2, "cluster + point_count");
        assert_eq!(values, 3, "true, 1234, false -- interned, not repeated");
        assert_eq!(extent, 4096);
        assert_eq!(version, 2);
    }

    #[test]
    fn interns_repeated_values() {
        let mut l = Layer::new("c", 4096);
        for i in 0..1000 {
            l.add_point(i, i as i32, 0, &[("point_count", Val::Uint(5))]);
        }
        assert_eq!(
            l.values.len(),
            1,
            "one distinct count should intern to one value"
        );
        assert_eq!(l.keys.len(), 1);
    }

    #[test]
    fn zigzag_handles_negative_coordinates() {
        let mut l = Layer::new("c", 4096);
        l.add_point(1, -1, -1, &[]);
        let tile = encode(vec![l]);
        // -1 zigzags to 1; the geometry payload must contain [9, 1, 1]
        assert!(
            tile.windows(3).any(|w| w == [9, 1, 1]),
            "negative coordinates did not zigzag correctly"
        );
    }
}
