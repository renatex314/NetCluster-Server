//! On-disk snapshots.
//!
//! # What is saved, and what is not
//!
//! Not the index. The tree is *derived* -- it rebuilds from device positions at
//! about a microsecond each -- so a snapshot stores only what cannot be
//! recomputed: the collection's geometry, and for each device its id, position,
//! category and last report time. That is roughly 30 bytes per device against 180
//! in memory, it reloads a million devices in about a second, and it is immune to
//! changes in the arena layout. Serialising the tree would be bigger, slower, and
//! would break every time the internals moved.
//!
//! One consequence worth knowing: a restore inserts in snapshot order rather than
//! the original operation order, so the tree shape -- and therefore `cluster_id`
//! values -- differ after a restart. Cluster ids are already documented as
//! invalidated by mutations, so nothing contractual changes, but marker groupings
//! may reshuffle slightly. That is strictly better than the alternative, where
//! they all vanish.
//!
//! # Format
//!
//! ```text
//! magic     "NCSNAP" + u16 version
//! meta      u32 length + JSON  { name, config }
//! count     u64
//! records   count x { u16 id_len, id bytes, i32 x, i32 y, u32 cat, u64 last_seen_ms }
//! checksum  u64 FNV-1a over everything above
//! ```
//!
//! Config travels as JSON so the format is self-describing and gains fields
//! without a version bump; records stay binary because there are a lot of them.

use crate::collection::Config;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 6] = b"NCSNAP";
const VERSION: u16 = 1;
const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

/// What the file says it is.
///
/// The collection name lives in the file rather than being recovered from the
/// filename: encoding is lossy for very long names, and a file that cannot say
/// what it is for is a file you cannot safely restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub config: Config,
}

/// One device, as stored. Coordinates are fixed-point Web Mercator, not degrees:
/// restoring through the projection would re-round every position on every
/// restart, and the drift would accumulate.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRecord {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub cat: u32,
    pub last_seen_ms: u64,
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= b as u64;
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// Wraps a writer and hashes everything on its way through, so the checksum costs
/// no second pass and no second copy of a 30 MB payload.
struct HashWriter<W: Write> {
    inner: W,
    hash: u64,
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        fnv(&mut self.hash, &buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A collection name is a URL path segment, so it arrives arbitrary: `../../etc/passwd`
/// is a perfectly legal name and a path traversal if used as a filename.
///
/// Everything outside `[A-Za-z0-9._-]` is percent-encoded, and a *leading* dot is
/// encoded too -- which is what rules out `.` and `..`, both of which are made
/// entirely of otherwise-allowed characters. Long names are truncated with a hash
/// appended, because most filesystems stop at 255 bytes.
pub fn encode_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for (i, b) in name.bytes().enumerate() {
        let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i > 0);
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.len() > 200 {
        let mut h = FNV_OFFSET;
        fnv(&mut h, name.as_bytes());
        out.truncate(180);
        out.push_str(&format!("~{h:016x}"));
    }
    if out.is_empty() {
        out.push_str("%00"); // an empty collection name is legal over HTTP
    }
    out
}

pub fn path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.ncs", encode_name(name)))
}

/// Write a snapshot, atomically.
///
/// Goes to `<path>.tmp`, is flushed to the platter, and only then renamed over the
/// real path. Rename is atomic within a filesystem, so a crash halfway through
/// leaves the previous good snapshot untouched rather than a half-written one.
///
/// Returns the size in bytes.
pub fn write(path: &Path, meta: &Meta, records: &[DeviceRecord]) -> io::Result<u64> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("ncs.tmp");
    let cfg = serde_json::to_vec(meta).map_err(io::Error::other)?;

    {
        let file = fs::File::create(&tmp)?;
        let mut w = HashWriter {
            inner: io::BufWriter::new(file),
            hash: FNV_OFFSET,
        };
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(cfg.len() as u32).to_le_bytes())?;
        w.write_all(&cfg)?;
        w.write_all(&(records.len() as u64).to_le_bytes())?;
        for r in records {
            let id = r.id.as_bytes();
            w.write_all(&(id.len() as u16).to_le_bytes())?;
            w.write_all(id)?;
            w.write_all(&r.x.to_le_bytes())?;
            w.write_all(&r.y.to_le_bytes())?;
            w.write_all(&r.cat.to_le_bytes())?;
            w.write_all(&r.last_seen_ms.to_le_bytes())?;
        }
        let sum = w.hash;
        w.write_all(&sum.to_le_bytes())?;
        w.flush()?;
        // Into the file, not just the OS buffer: a snapshot that only exists in
        // page cache is no use after the kind of crash this exists for.
        w.inner.into_inner().map_err(io::Error::other)?.sync_all()?;
    }

    fs::rename(&tmp, path)?;
    fs::metadata(path).map(|m| m.len())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Read a snapshot back.
///
/// The file is read whole and the checksum verified *before* anything is parsed.
/// A snapshot that fails here is corrupt, and the caller starts that collection
/// empty: serving half a fleet as though it were whole is worse than serving none
/// of it, because nothing downstream can tell the difference.
pub fn read(path: &Path) -> io::Result<(Meta, Vec<DeviceRecord>)> {
    let mut buf = Vec::new();
    fs::File::open(path)?.read_to_end(&mut buf)?;
    if buf.len() < MAGIC.len() + 2 + 4 + 8 + 8 {
        return Err(bad("snapshot is too short to be valid"));
    }
    let (body, tail) = buf.split_at(buf.len() - 8);
    let stored = u64::from_le_bytes(tail.try_into().unwrap());
    let mut h = FNV_OFFSET;
    fnv(&mut h, body);
    if h != stored {
        return Err(bad(
            "snapshot checksum mismatch: the file is corrupt or truncated",
        ));
    }

    let mut p = 0usize;
    let mut take = |n: usize| -> io::Result<&[u8]> {
        if p + n > body.len() {
            return Err(bad("snapshot ended mid-record"));
        }
        let s = &body[p..p + n];
        p += n;
        Ok(s)
    };

    if take(MAGIC.len())? != MAGIC {
        return Err(bad("not a netcluster snapshot"));
    }
    let version = u16::from_le_bytes(take(2)?.try_into().unwrap());
    if version != VERSION {
        return Err(bad(&format!(
            "snapshot format version {version}, this build understands {VERSION}"
        )));
    }
    let cfg_len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let meta: Meta = serde_json::from_slice(take(cfg_len)?).map_err(io::Error::other)?;
    let count = u64::from_le_bytes(take(8)?.try_into().unwrap()) as usize;

    // The count comes from the file, so it cannot be trusted to size an allocation
    // before the records have actually been read.
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let id_len = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        let id = String::from_utf8(take(id_len)?.to_vec())
            .map_err(|_| bad("device id is not valid UTF-8"))?;
        let x = i32::from_le_bytes(take(4)?.try_into().unwrap());
        let y = i32::from_le_bytes(take(4)?.try_into().unwrap());
        let cat = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let last_seen_ms = u64::from_le_bytes(take(8)?.try_into().unwrap());
        out.push(DeviceRecord {
            id,
            x,
            y,
            cat,
            last_seen_ms,
        });
    }
    if p != body.len() {
        return Err(bad("snapshot has trailing bytes"));
    }
    Ok((meta, out))
}

/// Delete a collection's snapshot. Missing is success: the caller wants it gone.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Every snapshot in a directory.
pub fn list(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) == Some("ncs") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ncsnap-{tag}-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn meta() -> Meta {
        Meta {
            name: "fleet".into(),
            config: cfg(),
        }
    }

    fn cfg() -> Config {
        Config {
            max_zoom: 14,
            radius: 50.0,
            extent: 512.0,
            hysteresis: 0.3,
            categories: vec!["idle".into(), "enroute".into()],
            ttl_seconds: 120,
        }
    }

    fn rec(id: &str, x: i32) -> DeviceRecord {
        DeviceRecord {
            id: id.to_string(),
            x,
            y: x / 2,
            cat: (x as u32) % 2,
            last_seen_ms: 1_700_000_000_000 + x as u64,
        }
    }

    #[test]
    fn round_trips_including_awkward_ids() {
        let d = tmpdir("round");
        let p = path_for(&d, "fleet");
        let records = vec![
            rec("truck-1", 100),
            rec("ônibus-2", 200),       // non-ASCII
            rec("", 300),               // empty id is legal over HTTP
            rec("a b/c?d#e", 400),      // characters that matter in a URL
            rec(&"x".repeat(500), 500), // longer than any sane id
        ];
        let n = write(&p, &meta(), &records).unwrap();
        assert!(n > 0);
        let (m2, r2) = read(&p).unwrap();
        assert_eq!(m2, meta(), "metadata did not survive the round trip");
        assert_eq!(r2, records, "records did not survive the round trip");
    }

    #[test]
    fn round_trips_a_large_fleet() {
        let d = tmpdir("large");
        let p = path_for(&d, "big");
        let records: Vec<_> = (0..100_000).map(|i| rec(&format!("v{i}"), i)).collect();
        let bytes = write(&p, &meta(), &records).unwrap();
        let (_, r2) = read(&p).unwrap();
        assert_eq!(r2.len(), 100_000);
        assert_eq!(r2[99_999], records[99_999]);
        // the size claim in the module docs, checked rather than asserted in prose
        let per = bytes as f64 / 100_000.0;
        assert!(per < 40.0, "{per:.1} bytes per device");
    }

    #[test]
    fn an_empty_collection_still_round_trips() {
        let d = tmpdir("empty");
        let p = path_for(&d, "nobody");
        write(&p, &meta(), &[]).unwrap();
        let (m2, r2) = read(&p).unwrap();
        assert_eq!(m2, meta(), "metadata must survive even with no devices");
        assert_eq!(m2.name, "fleet", "the file must say which collection it is");
        assert!(r2.is_empty());
    }

    /// Corruption must be caught before anything is parsed. Serving half a fleet as
    /// though it were whole is worse than serving none of it, because nothing
    /// downstream can tell the difference.
    #[test]
    fn truncation_and_bit_flips_are_caught() {
        let d = tmpdir("corrupt");
        let p = path_for(&d, "fleet");
        let records: Vec<_> = (0..100).map(|i| rec(&format!("v{i}"), i)).collect();
        write(&p, &meta(), &records).unwrap();
        let good = fs::read(&p).unwrap();

        for cut in [0, 1, 8, 20, good.len() / 2, good.len() - 1] {
            fs::write(&p, &good[..cut]).unwrap();
            assert!(read(&p).is_err(), "truncation to {cut} bytes was accepted");
        }

        for at in [10, 40, good.len() / 2, good.len() - 9] {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            fs::write(&p, &bad).unwrap();
            assert!(read(&p).is_err(), "a flipped bit at {at} was accepted");
        }

        // and appended garbage
        let mut extra = good.clone();
        extra.extend_from_slice(b"junk");
        fs::write(&p, &extra).unwrap();
        assert!(read(&p).is_err(), "trailing bytes were accepted");

        fs::write(&p, &good).unwrap();
        assert!(read(&p).is_ok(), "the untouched file should still load");
    }

    #[test]
    fn rejects_a_foreign_file() {
        let d = tmpdir("foreign");
        let p = d.join("x.ncs");
        fs::write(&p, b"this is not a snapshot, it is a text file").unwrap();
        assert!(read(&p).is_err());
    }

    /// Collection names arrive as URL path segments, so they are arbitrary.
    #[test]
    fn names_cannot_escape_the_directory() {
        let d = tmpdir("traversal");
        for name in [
            "../../etc/passwd",
            "..",
            ".",
            "/etc/shadow",
            "..\\..\\windows",
            ".hidden",
            "",
            "a/../../b",
        ] {
            let p = path_for(&d, name);
            let parent = p.parent().unwrap();
            assert_eq!(parent, d, "{name:?} escaped to {}", parent.display());
            // and it must actually be writable under that name
            write(&p, &meta(), &[rec("a", 1)]).unwrap();
            assert!(p.exists());
            let canon = p.canonicalize().unwrap();
            assert!(
                canon.starts_with(d.canonicalize().unwrap()),
                "{name:?} resolved outside the data directory: {}",
                canon.display()
            );
        }
    }

    #[test]
    fn distinct_names_get_distinct_files() {
        assert_ne!(encode_name("fleet"), encode_name("fleet2"));
        assert_ne!(encode_name(".."), encode_name("."));
        assert_ne!(encode_name("a/b"), encode_name("a_b"));
        assert_eq!(encode_name("plain-name_1.v2"), "plain-name_1.v2");
        // very long names stay within the filesystem limit and stay distinct
        let a = encode_name(&"z".repeat(4000));
        let b = encode_name(&format!("{}y", "z".repeat(3999)));
        assert!(a.len() < 255 && b.len() < 255, "{} {}", a.len(), b.len());
        assert_ne!(a, b, "truncation collapsed two different names");
    }

    /// A half-written snapshot must never replace a good one.
    #[test]
    fn a_failed_write_leaves_the_previous_snapshot_intact() {
        let d = tmpdir("atomic");
        let p = path_for(&d, "fleet");
        let first: Vec<_> = (0..50).map(|i| rec(&format!("v{i}"), i)).collect();
        write(&p, &meta(), &first).unwrap();

        // a stray temp file from an interrupted write must not be picked up
        fs::write(p.with_extension("ncs.tmp"), b"garbage").unwrap();
        let (_, r) = read(&p).unwrap();
        assert_eq!(r.len(), 50, "the good snapshot was disturbed");
        assert_eq!(
            list(&d).unwrap().len(),
            1,
            "the .tmp file was listed as a snapshot"
        );
    }

    #[test]
    fn listing_and_removal() {
        let d = tmpdir("list");
        assert!(list(&d).unwrap().is_empty());
        for n in ["a", "b", "c"] {
            write(&path_for(&d, n), &meta(), &[]).unwrap();
        }
        assert_eq!(list(&d).unwrap().len(), 3);
        remove(&path_for(&d, "b")).unwrap();
        assert_eq!(list(&d).unwrap().len(), 2);
        remove(&path_for(&d, "b")).unwrap(); // removing twice is not an error
        assert!(list(&std::env::temp_dir().join("ncsnap-does-not-exist"))
            .unwrap()
            .is_empty());
    }
}
