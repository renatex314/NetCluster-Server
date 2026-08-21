//! Open-addressed hash map from a packed cell key to a `u32`, backed by flat
//! `Vec`s. This is the bucket directory of the per-level grids.
//!
//! Why not `HashMap`: we need millions of entries, predictable memory, and tens
//! of millions of lookups per second on a key that is already a well-distributed
//! integer. `HashMap` would hash it again with SipHash and box the layout.
//!
//! Deletion uses backward-shift (Knuth 6.4 algorithm R) rather than tombstones.
//! That is not a micro-optimisation: the entire premise of this index is that it
//! runs forever without a rebuild, and a tombstoned table degrades until you
//! rebuild it.
//!
//! The hash function is a deliberate, bit-exact port of the JavaScript original,
//! so both implementations walk identical probe sequences. That makes a
//! divergence between them a real difference in the algorithm rather than an
//! artifact of table layout, which is what the differential test relies on.

const EMPTY: u64 = u64::MAX;

#[derive(Clone, Debug)]
pub struct CellHash {
    keys: Vec<u64>,
    vals: Vec<u32>,
    cap: usize,
    mask: usize,
    len: usize,
    limit: usize,
}

impl CellHash {
    pub fn with_capacity(initial: usize) -> Self {
        let mut cap = 8usize;
        while cap < initial {
            cap <<= 1;
        }
        let mut h = CellHash {
            keys: Vec::new(),
            vals: Vec::new(),
            cap: 0,
            mask: 0,
            len: 0,
            limit: 0,
        };
        h.alloc(cap);
        h
    }

    fn alloc(&mut self, cap: usize) {
        self.cap = cap;
        self.mask = cap - 1;
        self.keys = vec![EMPTY; cap];
        self.vals = vec![0u32; cap];
        self.len = 0;
        self.limit = (cap as f64 * 0.6) as usize;
    }

    /// Split the key into 32-bit halves and mix. Bit-for-bit the JavaScript
    /// `Math.imul`-based mixer; `wrapping_mul` on `u32` produces the same bits
    /// as a 32-bit signed multiply.
    #[inline]
    fn hash(key: u64) -> u32 {
        let lo = (key & 0xFFFF_FFFF) as u32;
        let hi = (key >> 32) as u32;
        let mut h = lo.wrapping_mul(0x9e37_79b1) ^ hi.wrapping_mul(0x85eb_ca6b);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2545_f491);
        h ^= h >> 13;
        h
    }

    #[inline]
    fn home(&self, key: u64) -> usize {
        (Self::hash(key) as usize) & self.mask
    }

    #[inline]
    pub fn get(&self, key: u64) -> Option<u32> {
        let mut i = self.home(key);
        loop {
            let k = self.keys[i];
            if k == key {
                return Some(self.vals[i]);
            }
            if k == EMPTY {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn set(&mut self, key: u64, val: u32) {
        debug_assert!(key != EMPTY, "EMPTY is reserved as the vacant sentinel");
        let mut i = self.home(key);
        loop {
            let k = self.keys[i];
            if k == key {
                self.vals[i] = val;
                return;
            }
            if k == EMPTY {
                self.keys[i] = key;
                self.vals[i] = val;
                self.len += 1;
                if self.len > self.limit {
                    self.grow();
                }
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn remove(&mut self, key: u64) -> bool {
        let mut i = self.home(key);
        loop {
            let k = self.keys[i];
            if k == key {
                break;
            }
            if k == EMPTY {
                return false;
            }
            i = (i + 1) & self.mask;
        }
        // Backward-shift: pull back any entry that probed past slot `i`, so the
        // table is left exactly as if the removed key had never been inserted.
        let mut j = i;
        loop {
            self.keys[i] = EMPTY;
            loop {
                j = (j + 1) & self.mask;
                if self.keys[j] == EMPTY {
                    self.len -= 1;
                    return true;
                }
                let home = self.home(self.keys[j]);
                // is `home` cyclically outside (i, j]? then entry j may fill hole i
                let a = j.wrapping_sub(i) & self.mask;
                let b = j.wrapping_sub(home) & self.mask;
                if b >= a {
                    break;
                }
            }
            self.keys[i] = self.keys[j];
            self.vals[i] = self.vals[j];
            i = j;
        }
    }

    fn grow(&mut self) {
        let old_keys = std::mem::take(&mut self.keys);
        let old_vals = std::mem::take(&mut self.vals);
        let old_cap = self.cap;
        self.alloc(old_cap << 1);
        for i in 0..old_cap {
            if old_keys[i] != EMPTY {
                self.set(old_keys[i], old_vals[i]);
            }
        }
    }

    /// Every occupied `(key, value)` pair, in table order.
    ///
    /// Table order is an implementation detail; this exists so the invariant
    /// checker can audit the grid directly rather than through the index.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.keys
            .iter()
            .zip(self.vals.iter())
            .filter(|(k, _)| **k != EMPTY)
            .map(|(k, v)| (*k, *v))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn bytes(&self) -> usize {
        self.cap * (8 + 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_lookup_remove_round_trip() {
        let mut h = CellHash::with_capacity(8);
        for i in 0..5000u64 {
            h.set(i * 7 + 1, i as u32);
        }
        assert_eq!(h.len(), 5000);
        for i in 0..5000u64 {
            assert_eq!(h.get(i * 7 + 1), Some(i as u32));
        }
        assert_eq!(h.get(999_999_999), None);
        for i in (0..5000u64).step_by(2) {
            assert!(h.remove(i * 7 + 1));
        }
        for i in 0..5000u64 {
            let want = if i % 2 == 0 { None } else { Some(i as u32) };
            assert_eq!(h.get(i * 7 + 1), want, "key {i}");
        }
    }

    /// Backward-shift deletion must leave no debris: churning far more entries
    /// than the table ever holds at once must not grow it.
    #[test]
    fn churn_does_not_accumulate_garbage() {
        let mut h = CellHash::with_capacity(1024);
        let cap_before = h.cap;
        for round in 0..200u64 {
            for i in 0..300u64 {
                h.set(round * 1000 + i, i as u32);
            }
            for i in 0..300u64 {
                assert!(h.remove(round * 1000 + i));
            }
        }
        assert_eq!(h.len(), 0);
        assert_eq!(
            h.cap, cap_before,
            "table grew despite a steady-state size of 0"
        );
    }
}
