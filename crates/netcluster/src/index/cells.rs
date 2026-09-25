//! Per-node filter aggregates, in whichever layout fits.
//!
//! A *cell* is one combination of filter values. Every node in the tree carries
//! `(count, sum_x, sum_y)` for each cell occurring beneath it, so a filtered
//! viewport query reads a precomputed total instead of walking the subtree.
//!
//! Two layouts, one interface:
//!
//! - **Dense**, `slot * cells + cell`. An array index, and what the single
//!   `categories` slice has always been. Costs 20 bytes per slot per cell whether
//!   or not the cell occurs.
//! - **Sparse**, keyed `(slot, cell)` in a [`CellHash`], with each node's entries
//!   also on its own doubly-linked list so a re-homed subtree's cells can be
//!   enumerated.
//!
//! The dense product is why the sparse layout exists at all. Conjunctive filters
//! need a cell per *combination* -- marginal counts cannot answer `count(A and
//! B)` -- and three dimensions of 40, 8 and 5 values is 1,600 cells, or 6.4 TB of
//! mostly-zero slices at 200k devices. A node's subtree holds at most `count`
//! distinct cells, so storing only what occurs needs one entry per device per
//! shape per level: 1.5M entries on a 200k fleet, and flat as dimensions are
//! added.
//!
//! `find` returns an index into the same three vectors either way, so every
//! reader is written once.

use crate::cellhash::CellHash;

/// Empty link. Distinct from the crate-level `NONE` slot sentinel: this indexes
/// entries, not slots.
const EMPTY: i32 = -1;

/// Cell space reserved per slot in the sparse key. Slots are below 2^32 and
/// cells below 2^24, so `slot << 24 | cell` fits a u64 with room to spare.
const CELL_BITS: u32 = 24;
pub const MAX_CELLS: usize = 1 << CELL_BITS;

pub(crate) struct CellTable {
    pub cells: usize,
    pub dense: bool,
    cnt: Vec<i32>,
    sx: Vec<i64>,
    sy: Vec<i64>,
    // sparse only
    agg: CellHash,
    cell_of: Vec<u32>,
    next: Vec<i32>,
    prev: Vec<i32>,
    head: Vec<i32>,
    n: u32,
    free: i32,
    /// Reused by `move_subtree` so re-homing allocates nothing.
    scratch: Vec<(u32, i32, i64, i64)>,
}

impl CellTable {
    pub fn new(cells: usize, dense_cells: usize) -> Self {
        let dense = cells > 0 && cells <= dense_cells;
        Self {
            cells,
            dense,
            cnt: Vec::new(),
            sx: Vec::new(),
            sy: Vec::new(),
            agg: CellHash::with_capacity(if dense { 1 } else { 1024 }),
            cell_of: Vec::new(),
            next: Vec::new(),
            prev: Vec::new(),
            head: Vec::new(),
            n: 0,
            free: EMPTY,
            scratch: Vec::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cells > 0
    }

    #[inline]
    fn key(s: u32, cell: u32) -> u64 {
        ((s as u64) << CELL_BITS) | cell as u64
    }

    /// Grow to `cap` slots. Dense storage is slot-major and grows with the arena;
    /// sparse storage grows with occupancy instead, so only the per-node heads do.
    pub fn grow(&mut self, cap: usize) {
        if !self.enabled() {
            return;
        }
        if self.dense {
            self.cnt.resize(cap * self.cells, 0);
            self.sx.resize(cap * self.cells, 0);
            self.sy.resize(cap * self.cells, 0);
        } else {
            self.head.resize(cap, EMPTY);
        }
    }

    #[inline]
    pub fn find(&self, s: u32, cell: u32) -> Option<usize> {
        if self.dense {
            Some(s as usize * self.cells + cell as usize)
        } else {
            self.agg.get(Self::key(s, cell)).map(|e| e as usize)
        }
    }

    #[inline]
    pub fn count(&self, s: u32, cell: u32) -> i32 {
        match self.find(s, cell) {
            Some(e) => self.cnt[e],
            None => 0,
        }
    }

    #[inline]
    pub fn at(&self, e: usize) -> (i32, i64, i64) {
        (self.cnt[e], self.sx[e], self.sy[e])
    }

    /// The `(s, cell)` entry, created empty and linked onto `s` if absent.
    fn entry(&mut self, s: u32, cell: u32) -> usize {
        if self.dense {
            return s as usize * self.cells + cell as usize;
        }
        let key = Self::key(s, cell);
        if let Some(e) = self.agg.get(key) {
            return e as usize;
        }
        let e = if self.free != EMPTY {
            let e = self.free as usize;
            self.free = self.next[e];
            e
        } else {
            let e = self.n as usize;
            self.n += 1;
            self.cnt.push(0);
            self.sx.push(0);
            self.sy.push(0);
            self.cell_of.push(0);
            self.next.push(EMPTY);
            self.prev.push(EMPTY);
            e
        };
        self.cnt[e] = 0;
        self.sx[e] = 0;
        self.sy[e] = 0;
        self.cell_of[e] = cell;
        let head = self.head[s as usize];
        self.next[e] = head;
        self.prev[e] = EMPTY;
        if head != EMPTY {
            self.prev[head as usize] = e as i32;
        }
        self.head[s as usize] = e as i32;
        self.agg.set(key, e as u32);
        e
    }

    /// Release one entry. Entries go the moment their count reaches zero rather
    /// than being left at zero: an index that churns for months would otherwise
    /// accumulate a row per cell each node ever held.
    fn release(&mut self, s: u32, e: usize) {
        let (p, nx) = (self.prev[e], self.next[e]);
        if p == EMPTY {
            self.head[s as usize] = nx;
        } else {
            self.next[p as usize] = nx;
        }
        if nx != EMPTY {
            self.prev[nx as usize] = p;
        }
        self.agg.remove(Self::key(s, self.cell_of[e]));
        self.next[e] = self.free;
        self.free = e as i32;
    }

    #[inline]
    pub fn bump(&mut self, s: u32, cell: u32, dc: i32, dx: i64, dy: i64) {
        if self.dense {
            let i = s as usize * self.cells + cell as usize;
            self.cnt[i] += dc;
            self.sx[i] += dx;
            self.sy[i] += dy;
            return;
        }
        let e = self.entry(s, cell);
        self.cnt[e] += dc;
        self.sx[e] += dx;
        self.sy[e] += dy;
        if self.cnt[e] == 0 {
            self.release(s, e);
        }
    }

    /// Drop everything `s` holds.
    pub fn drop_slot(&mut self, s: u32) {
        if !self.enabled() {
            return;
        }
        if self.dense {
            let b = s as usize * self.cells;
            for k in 0..self.cells {
                self.cnt[b + k] = 0;
                self.sx[b + k] = 0;
                self.sy[b + k] = 0;
            }
            return;
        }
        let mut e = self.head[s as usize];
        while e != EMPTY {
            let ei = e as usize;
            let nx = self.next[ei];
            self.agg.remove(Self::key(s, self.cell_of[ei]));
            self.next[ei] = self.free;
            self.free = e;
            e = nx;
        }
        self.head[s as usize] = EMPTY;
    }

    /// Reset `s` to carry exactly one point, in each of `cells`.
    pub fn set_self(&mut self, s: u32, cells: &[u32], x: i64, y: i64) {
        if !self.enabled() {
            return;
        }
        self.drop_slot(s);
        for &c in cells {
            let e = self.entry(s, c);
            self.cnt[e] = 1;
            self.sx[e] = x;
            self.sy[e] = y;
        }
    }

    /// Apply one device's `cells` to every node on `chain`.
    pub fn walk(&mut self, chain: &[u32], cells: &[u32], dc: i32, dx: i64, dy: i64) {
        if !self.enabled() {
            return;
        }
        for &t in chain {
            for &c in cells {
                self.bump(t, c, dc, dx, dy);
            }
        }
    }

    /// Move a whole subtree's cell mass on or off `chain`.
    ///
    /// Costs the cells that subtree actually holds -- at most its point count --
    /// rather than every declared cell, which is what the dense layout paid
    /// unconditionally.
    pub fn move_subtree(&mut self, chain: &[u32], node: u32, sign: i32) {
        if !self.enabled() {
            return;
        }
        self.scratch.clear();
        if self.dense {
            let nb = node as usize * self.cells;
            for k in 0..self.cells {
                let n = self.cnt[nb + k];
                if n != 0 {
                    self.scratch
                        .push((k as u32, n, self.sx[nb + k], self.sy[nb + k]));
                }
            }
        } else {
            let mut e = self.head[node as usize];
            while e != EMPTY {
                let ei = e as usize;
                self.scratch
                    .push((self.cell_of[ei], self.cnt[ei], self.sx[ei], self.sy[ei]));
                e = self.next[ei];
            }
        }
        // Taken so the borrow checker sees `bump`'s &mut self as disjoint; the
        // buffer is put back for the next call.
        let moved = std::mem::take(&mut self.scratch);
        for &t in chain {
            for &(cell, c, x, y) in &moved {
                self.bump(t, cell, sign * c, sign as i64 * x, sign as i64 * y);
            }
        }
        self.scratch = moved;
    }

    /// Live entries -- what the filter is actually costing.
    pub fn entries(&self) -> usize {
        if !self.enabled() {
            return 0;
        }
        if self.dense {
            return self.cnt.iter().filter(|&&c| c != 0).count();
        }
        let mut free = 0usize;
        let mut e = self.free;
        while e != EMPTY {
            free += 1;
            e = self.next[e as usize];
        }
        self.n as usize - free
    }

    pub fn bytes(&self) -> usize {
        if !self.enabled() {
            return 0;
        }
        if self.dense {
            self.cnt.capacity() * 4 + self.sx.capacity() * 8 + self.sy.capacity() * 8
        } else {
            self.cnt.capacity() * 4
                + self.sx.capacity() * 8
                + self.sy.capacity() * 8
                + self.cell_of.capacity() * 4
                + self.next.capacity() * 4
                + self.prev.capacity() * 4
                + self.head.capacity() * 4
                + self.agg.bytes()
        }
    }
}
