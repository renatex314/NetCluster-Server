//! The index itself.

use crate::cellhash::CellHash;
use crate::feature::{ClusterIdError, Feature, TileFeature};
use crate::project::{project, unproject, PREC};
use std::cmp::Ordering;
use std::collections::HashMap;

mod verify;
pub use verify::Verification;

/// A node handle. Slots are dense indices into the parallel arrays and are
/// recycled through a free list.
pub type Slot = u32;

/// "no slot" -- the sentinel that terminates every parent, child and sibling
/// chain.
pub const NONE: Slot = u32::MAX;

/// A slot that is on the free list rather than in the tree.
const DEAD: i8 = -2;

const MAX_CELL_BITS: u32 = 24;
const KEY_Y: u64 = 1 << MAX_CELL_BITS;
const KEY_X: u64 = 1 << (MAX_CELL_BITS * 2);

/// Geometry of the hierarchy. Every process sharing an index must agree on all
/// of it; changing any field changes what the clusters mean.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Coarsest zoom a query may ask for.
    pub min_zoom: u8,
    /// Finest zoom at which points are still clustered. Above it, every point
    /// stands alone. Hard-capped at 20 by the fixed-point cell resolution.
    pub max_zoom: u8,
    /// Cluster radius in screen pixels, at `extent` pixels per tile. 40 is
    /// supercluster's default and produces comparable output.
    pub radius: f64,
    /// Tile extent in pixels that `radius` is measured against.
    pub extent: f64,
    /// Covering slack. An existing parent assignment survives until it is
    /// violated by this factor, trading a slightly looser radius bound
    /// (`2(1+h)·r_z`) for far fewer visible cluster changes under continuous
    /// motion. 0 is the textbook net; 0.25 is what a map wants.
    pub hysteresis: f64,
    /// Number of categories to keep separate aggregate slices for, enabling
    /// filtered queries. 0 disables the machinery entirely.
    ///
    /// Cost note: a point belongs to exactly one category, so it touches exactly
    /// one slice per level. Update cost does **not** grow with this number.
    pub categories: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            min_zoom: 0,
            max_zoom: 16,
            radius: 40.0,
            extent: 512.0,
            hysteresis: 0.25,
            categories: 0,
        }
    }
}

/// Counters, for benchmarks and for watching an index behave in production.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub inserts: u64,
    pub removes: u64,
    pub moves: u64,
    /// Moves absorbed by the fast path: no invariant was broken, so nothing but
    /// aggregates and possibly a grid cell changed.
    pub moves_fast: u64,
    /// Moves that needed a local repair (detach, re-home children, re-link).
    pub moves_rebuilt: u64,
    /// Re-homed children that ended up at a coarser level than before.
    pub promotions: u64,
    pub reparents: u64,
    pub probes: u64,
}

/// Fully dynamic hierarchical geospatial clustering.
///
/// The index maintains, simultaneously for every zoom level `z` in
/// `[0, max_zoom]`, a *net* of the live point set at scale
/// `r_z = radius / (extent · 2^z)`:
///
/// - **Nesting**  `C_0 ⊆ C_1 ⊆ … ⊆ C_maxZoom ⊆ P`
/// - **Separation**  distinct `c, c'` in `C_z` are more than `r_z` apart
/// - **Covering**  every `p` in `C_{z+1} \ C_z` has a parent `q` in `C_z` with `d(p,q) ≤ r_z`
///
/// Those are the invariants of a compressed net-tree (equivalently a cover tree)
/// over the Web-Mercator plane, and they are repaired *locally* on every update.
/// They give two guarantees that hold at every level at every moment, with no
/// global recomputation ever:
///
/// - cluster radius `d(p, rep_z(p)) ≤ Σ_{j≥z} r_j ≤ 2·r_z` (a geometric series)
/// - cluster count `|C_z| ≤ |OPT(r_z/2)|`, because an `r_z`-separated set puts at
///   most one member in any ball of radius `r_z/2`
///
/// so the level-`z` clustering is permanently a bicriteria (2, 1)-approximation
/// of the optimal radius-`r_z` clustering.
///
/// # Representation
///
/// One node per point. A point records the *coarsest* level at which it is a
/// center (`tz`); it is implicitly a center at every finer level too, which is
/// what avoids materialising every level and the `O(N log Δ)` blowup that would
/// come with it. `tz == max_zoom + 1` means "never a center".
///
/// # Example
///
/// ```
/// use netcluster::{NetCluster, Options, Feature};
///
/// let mut nc = NetCluster::new(Options::default());
/// nc.insert(1, -46.6333, -23.5505);
/// nc.insert(2, -46.6340, -23.5510);
/// nc.insert(3, -43.1729, -22.9068);
///
/// // Two São Paulo points merge at low zoom; Rio stays separate.
/// let world = nc.get_clusters([-80.0, -40.0, -30.0, 10.0], 4.0, -1);
/// assert_eq!(world.len(), 2);
///
/// // Moving one of them costs microseconds, not a rebuild.
/// nc.move_to(1, -46.6400, -23.5600);
/// assert_eq!(nc.len(), 3);
/// ```
pub struct NetCluster {
    min_zoom: u8,
    max_zoom: u8,
    radius: f64,
    extent: f64,
    hysteresis: f64,
    categories: usize,

    /// Cluster scale per level, in fixed-point units. `r[leaf] = -1` so nothing
    /// can ever be covered at the leaf level, which terminates the descent.
    r: Vec<f64>,
    r2: Vec<f64>,
    /// Grid cell side, `2·r_z`. The query ball has diameter `2·r_z`, so it spans
    /// at most two cells per axis: four probes per level instead of nine. Cells
    /// stay dyadic (`cs[z] == 2·cs[z+1]`), which is what makes the shift trick in
    /// `grid_move` correct.
    cs: Vec<f64>,
    hyst2: Vec<f64>,

    grid: CellHash,
    ids: HashMap<u64, Slot>,

    e_slot: Vec<Slot>,
    e_next: Vec<Slot>,
    e_n: u32,
    e_free: Slot,

    qx: Vec<i32>,
    qy: Vec<i32>,
    sx: Vec<i64>,
    sy: Vec<i64>,
    cnt: Vec<i32>,
    par: Vec<Slot>,
    kid: Vec<Slot>,
    sib: Vec<Slot>,
    psib: Vec<Slot>,
    tz: Vec<i8>,
    ext: Vec<u64>,

    cat: Vec<u32>,
    ccnt: Vec<i32>,
    csx: Vec<i64>,
    csy: Vec<i64>,

    n: u32,
    free_head: Slot,

    cand: Vec<Slot>,
    kids: Vec<Slot>,

    pub stats: Stats,
    d_level: i8,
    d_parent: Slot,
}

impl NetCluster {
    pub fn new(opts: Options) -> Self {
        assert!(
            opts.max_zoom <= 20,
            "max_zoom > 20 exceeds the fixed-point cell resolution"
        );
        assert!(
            opts.extent > 0.0 && opts.radius > 0.0,
            "radius and extent must be positive"
        );
        assert!(opts.hysteresis >= 0.0, "hysteresis must be non-negative");
        assert!(
            opts.min_zoom <= opts.max_zoom,
            "min_zoom {} must not exceed max_zoom {}",
            opts.min_zoom,
            opts.max_zoom
        );

        let leaf = opts.max_zoom as usize + 1; // index of the "not a center anywhere" level
        let mut r = vec![0.0f64; leaf + 1];
        let mut r2 = vec![0.0f64; leaf + 1];
        let mut cs = vec![0.0f64; leaf + 1];
        let mut hyst2 = vec![0.0f64; leaf + 1];
        for z in 0..=opts.max_zoom as usize {
            r[z] = PREC * opts.radius / (opts.extent * (1u64 << z) as f64);
            r2[z] = r[z] * r[z];
            cs[z] = 2.0 * r[z];
            let rr = r[z] * (1.0 + opts.hysteresis);
            hyst2[z] = rr * rr;
        }
        r[leaf] = -1.0;
        r2[leaf] = -1.0;

        let mut nc = NetCluster {
            min_zoom: opts.min_zoom,
            max_zoom: opts.max_zoom,
            radius: opts.radius,
            extent: opts.extent,
            hysteresis: opts.hysteresis,
            categories: opts.categories,
            r,
            r2,
            cs,
            hyst2,
            grid: CellHash::with_capacity(1024),
            ids: HashMap::new(),
            e_slot: vec![NONE; 1024],
            e_next: vec![NONE; 1024],
            e_n: 0,
            e_free: NONE,
            qx: Vec::new(),
            qy: Vec::new(),
            sx: Vec::new(),
            sy: Vec::new(),
            cnt: Vec::new(),
            par: Vec::new(),
            kid: Vec::new(),
            sib: Vec::new(),
            psib: Vec::new(),
            tz: Vec::new(),
            ext: Vec::new(),
            cat: Vec::new(),
            ccnt: Vec::new(),
            csx: Vec::new(),
            csy: Vec::new(),
            n: 0,
            free_head: NONE,
            cand: vec![NONE; 256],
            kids: Vec::with_capacity(64),
            stats: Stats::default(),
            d_level: 0,
            d_parent: NONE,
        };
        nc.grow(1024);
        nc
    }

    // ------------------------------------------------------------- storage --

    #[inline]
    fn cap(&self) -> usize {
        self.qx.len()
    }

    fn grow(&mut self, cap: usize) {
        self.qx.resize(cap, 0);
        self.qy.resize(cap, 0);
        self.sx.resize(cap, 0);
        self.sy.resize(cap, 0);
        self.cnt.resize(cap, 0);
        self.par.resize(cap, NONE);
        self.kid.resize(cap, NONE);
        self.sib.resize(cap, NONE);
        self.psib.resize(cap, NONE);
        self.tz.resize(cap, DEAD);
        self.ext.resize(cap, 0);
        if self.categories > 0 {
            self.cat.resize(cap, 0);
            self.ccnt.resize(cap * self.categories, 0);
            self.csx.resize(cap * self.categories, 0);
            self.csy.resize(cap * self.categories, 0);
        }
    }

    fn alloc_slot(&mut self) -> Slot {
        let s = if self.free_head != NONE {
            let s = self.free_head;
            self.free_head = self.par[s as usize];
            s
        } else {
            if self.n as usize == self.cap() {
                let c = self.cap() * 2;
                self.grow(c);
            }
            let s = self.n;
            self.n += 1;
            s
        };
        let si = s as usize;
        self.kid[si] = NONE;
        self.sib[si] = NONE;
        self.psib[si] = NONE;
        self.par[si] = NONE;
        // A recycled slot must not inherit stale slices.
        let k = self.categories;
        if k > 0 {
            let b = si * k;
            for i in 0..k {
                self.ccnt[b + i] = 0;
                self.csx[b + i] = 0;
                self.csy[b + i] = 0;
            }
        }
        s
    }

    /// Reset `s` to carry only its own point, in the total and in its slice.
    fn self_mass(&mut self, s: Slot) {
        let si = s as usize;
        let (x, y) = (self.qx[si] as i64, self.qy[si] as i64);
        self.cnt[si] = 1;
        self.sx[si] = x;
        self.sy[si] = y;
        let k = self.categories;
        if k > 0 {
            let b = si * k;
            for i in 0..k {
                self.ccnt[b + i] = 0;
                self.csx[b + i] = 0;
                self.csy[b + i] = 0;
            }
            let c = self.cat[si] as usize;
            self.ccnt[b + c] = 1;
            self.csx[b + c] = x;
            self.csy[b + c] = y;
        }
    }

    fn free_slot(&mut self, s: Slot) {
        let si = s as usize;
        self.par[si] = self.free_head;
        self.free_head = s;
        self.tz[si] = DEAD;
    }

    // ---------------------------------------------------------------- grid --
    //
    // One bucket list per (level, cell). A center of C_z is listed in the grid of
    // EVERY level z >= tz, which costs Σ_z |C_z| entries (measured 3.7-5.4 per
    // point on fleet data, hard-capped at max_zoom + 1) and buys the decisive
    // property: "is any center of C_z within r_z" is one small cell probe, so the
    // placement sweep can run bottom-up and stop at the first hit.

    fn grow_entries(&mut self) {
        let cap = self.e_slot.len() * 2;
        self.e_slot.resize(cap, NONE);
        self.e_next.resize(cap, NONE);
    }

    fn new_entry(&mut self, s: Slot, next: Slot) -> u32 {
        let e = if self.e_free != NONE {
            let e = self.e_free;
            self.e_free = self.e_next[e as usize];
            e
        } else {
            if self.e_n as usize == self.e_slot.len() {
                self.grow_entries();
            }
            let e = self.e_n;
            self.e_n += 1;
            e
        };
        self.e_slot[e as usize] = s;
        self.e_next[e as usize] = next;
        e
    }

    #[inline]
    fn key(z: i32, cx: i64, cy: i64) -> u64 {
        z as u64 * KEY_X + cx as u64 * KEY_Y + cy as u64
    }

    /// Cell index at the finest level; coarser levels are `>> (max_zoom - z)`.
    #[inline]
    fn cell_x(&self, s: Slot) -> i64 {
        (self.qx[s as usize] as f64 / self.cs[self.max_zoom as usize]).floor() as i64
    }

    #[inline]
    fn cell_y(&self, s: Slot) -> i64 {
        (self.qy[s as usize] as f64 / self.cs[self.max_zoom as usize]).floor() as i64
    }

    fn grid_add_at(&mut self, s: Slot, z: i32, cx: i64, cy: i64) {
        let k = Self::key(z, cx, cy);
        let head = self.grid.get(k).unwrap_or(NONE);
        let e = self.new_entry(s, head);
        self.grid.set(k, e);
    }

    fn grid_del_at(&mut self, s: Slot, z: i32, cx: i64, cy: i64) {
        let k = Self::key(z, cx, cy);
        let head = self.grid.get(k).expect("grid corruption: missing cell");
        if self.e_slot[head as usize] == s {
            let nx = self.e_next[head as usize];
            if nx == NONE {
                self.grid.remove(k);
            } else {
                self.grid.set(k, nx);
            }
            self.e_next[head as usize] = self.e_free;
            self.e_free = head;
            return;
        }
        let mut e = head;
        loop {
            let nx = self.e_next[e as usize];
            assert!(nx != NONE, "grid corruption: slot not in cell");
            if self.e_slot[nx as usize] == s {
                break;
            }
            e = nx;
        }
        let d = self.e_next[e as usize];
        self.e_next[e as usize] = self.e_next[d as usize];
        self.e_next[d as usize] = self.e_free;
        self.e_free = d;
    }

    /// List `s` in every level from `tz[s]` down to `max_zoom`.
    fn grid_add(&mut self, s: Slot) {
        let t = self.tz[s as usize] as i32;
        if t > self.max_zoom as i32 {
            return;
        }
        let (mut cx, mut cy) = (self.cell_x(s), self.cell_y(s));
        let mut z = self.max_zoom as i32;
        while z >= t {
            self.grid_add_at(s, z, cx, cy);
            cx >>= 1;
            cy >>= 1;
            z -= 1;
        }
    }

    fn grid_del(&mut self, s: Slot) {
        let t = self.tz[s as usize] as i32;
        if t > self.max_zoom as i32 {
            return;
        }
        let (mut cx, mut cy) = (self.cell_x(s), self.cell_y(s));
        let mut z = self.max_zoom as i32;
        while z >= t {
            self.grid_del_at(s, z, cx, cy);
            cx >>= 1;
            cy >>= 1;
            z -= 1;
        }
    }

    /// Reposition a listed center.
    ///
    /// Cells are dyadic and aligned, so the level-`z` cell is the level-`z+1`
    /// cell shifted right: once a level's cell is unchanged, every coarser level
    /// is unchanged too and the walk stops. A device creeping inside its own cell
    /// therefore touches no grid at all -- which is the common case, and the
    /// reason a move is microseconds.
    fn grid_move(&mut self, s: Slot, nx: i32, ny: i32) {
        let si = s as usize;
        let t = self.tz[si] as i32;
        if t > self.max_zoom as i32 {
            self.qx[si] = nx;
            self.qy[si] = ny;
            return;
        }
        let cs = self.cs[self.max_zoom as usize];
        let mut ox = (self.qx[si] as f64 / cs).floor() as i64;
        let mut oy = (self.qy[si] as f64 / cs).floor() as i64;
        let mut px = (nx as f64 / cs).floor() as i64;
        let mut py = (ny as f64 / cs).floor() as i64;
        let mut z = self.max_zoom as i32;
        while z >= t && (ox != px || oy != py) {
            self.grid_del_at(s, z, ox, oy);
            self.grid_add_at(s, z, px, py);
            ox >>= 1;
            oy >>= 1;
            px >>= 1;
            py >>= 1;
            z -= 1;
        }
        self.qx[si] = nx;
        self.qy[si] = ny;
    }

    /// Collect every center of `C_z` within `rad` of `(x, y)` into `self.cand`,
    /// returning how many. `rad <= cs[z]`, so this is a 2x2 (at most 3x3) block,
    /// and each cell holds O(1) centers because `C_z` is `r_z`-separated.
    fn scan(&mut self, z: i32, x: i32, y: i32, rad: f64, rad2: f64, exclude: Slot) -> usize {
        let cs = self.cs[z as usize];
        let maxc = (PREC / cs).ceil() as i64;
        let (xf, yf) = (x as f64, y as f64);
        let mut cx0 = ((xf - rad) / cs).floor() as i64;
        let mut cx1 = ((xf + rad) / cs).floor() as i64;
        let mut cy0 = ((yf - rad) / cs).floor() as i64;
        let mut cy1 = ((yf + rad) / cs).floor() as i64;
        if cx0 < 0 {
            cx0 = 0;
        }
        if cy0 < 0 {
            cy0 = 0;
        }
        if cx1 > maxc {
            cx1 = maxc;
        }
        if cy1 > maxc {
            cy1 = maxc;
        }
        let cap = self.cand.len();
        let mut n = 0usize;
        for cx in cx0..=cx1 {
            let base = z as u64 * KEY_X + cx as u64 * KEY_Y;
            for cy in cy0..=cy1 {
                self.stats.probes += 1;
                let mut e = match self.grid.get(base + cy as u64) {
                    Some(e) => e,
                    None => continue,
                };
                loop {
                    let s = self.e_slot[e as usize];
                    if s != exclude {
                        let dx = (self.qx[s as usize] - x) as i64;
                        let dy = (self.qy[s as usize] - y) as i64;
                        if (dx * dx + dy * dy) as f64 <= rad2 && n < cap {
                            self.cand[n] = s;
                            n += 1;
                        }
                    }
                    e = self.e_next[e as usize];
                    if e == NONE {
                        break;
                    }
                }
            }
        }
        n
    }

    /// Where does `(x, y)` belong?
    ///
    /// Sweep levels from the **finest upward** and stop at the first level whose
    /// net already covers the point: that is the finest covering level `z*`, so
    /// the point becomes a center at `z*+1` (the leaf level when `z* == max_zoom`)
    /// with the nearest `C_{z*}` member as its parent.
    ///
    /// Sweeping upward is what makes this cheap: it visits `max_zoom - tz + 2`
    /// levels, about four on real data, rather than all seventeen. Sweeping
    /// downward would be wrong as well as slower -- "covered at level z" is *not*
    /// monotone in z, because a finer level has a smaller radius but a larger
    /// center set, so an early stop from the coarse end can leave a point
    /// separated from the coarse net while a finer center sits inside its
    /// exclusion radius.
    fn descend(&mut self, x: i32, y: i32, exclude: Slot, from: Option<i32>) {
        let mz = self.max_zoom as i32;
        let top = match from {
            None => mz,
            Some(f) if f > mz => mz,
            Some(f) => f,
        };
        let mut z = top;
        while z >= 0 {
            let (rad, rad2) = (self.r[z as usize], self.r2[z as usize]);
            let n = self.scan(z, x, y, rad, rad2, exclude);
            if n == 0 {
                z -= 1;
                continue;
            }
            let mut bd = i64::MAX;
            let mut bs = NONE;
            for i in 0..n {
                let s = self.cand[i];
                let dx = (self.qx[s as usize] - x) as i64;
                let dy = (self.qy[s as usize] - y) as i64;
                let d2 = dx * dx + dy * dy;
                // Exact ties are broken on the id, so the structure is a function
                // of the point set and the operation order alone -- never of hash
                // or chain iteration order. Two implementations that agree on this
                // rule produce byte-identical output; that is what makes the
                // differential test against the JavaScript build meaningful.
                let better = bs == NONE
                    || d2 < bd
                    || (d2 == bd
                        && cmp_decimal(self.ext[s as usize], self.ext[bs as usize])
                            == Ordering::Less);
                if better {
                    bd = d2;
                    bs = s;
                }
            }
            self.d_level = (z + 1) as i8;
            self.d_parent = bs;
            return;
        }
        self.d_level = 0;
        self.d_parent = NONE;
    }

    /// Finest level `>= from` at which some center other than `exclude` covers
    /// `(x, y)`; `-1` if none does.
    fn covered_at_or_below(&mut self, x: i32, y: i32, from: i32, exclude: Slot) -> i32 {
        let mut z = self.max_zoom as i32;
        while z >= from {
            let (rad, rad2) = (self.r[z as usize], self.r2[z as usize]);
            if self.scan(z, x, y, rad, rad2, exclude) > 0 {
                return z;
            }
            z -= 1;
        }
        -1
    }

    // ---------------------------------------------------------- aggregates --

    /// Add the mass of ONE point to `s` and every ancestor.
    ///
    /// This is the hot path. A point belongs to a single category, so it touches
    /// a single slice, and the work is independent of how many categories exist.
    fn agg(&mut self, mut s: Slot, dc: i32, dx: i64, dy: i64, k: Option<usize>) {
        let kk = self.categories;
        if kk > 0 {
            if let Some(k) = k {
                while s != NONE {
                    let si = s as usize;
                    self.cnt[si] += dc;
                    self.sx[si] += dx;
                    self.sy[si] += dy;
                    let b = si * kk + k;
                    self.ccnt[b] += dc;
                    self.csx[b] += dx;
                    self.csy[b] += dy;
                    s = self.par[si];
                }
                return;
            }
        }
        while s != NONE {
            let si = s as usize;
            self.cnt[si] += dc;
            self.sx[si] += dx;
            self.sy[si] += dy;
            s = self.par[si];
        }
    }

    /// Move a whole subtree's mass (all slices) on or off an ancestor chain.
    ///
    /// Only re-homing does this, about 3.3 times per removal, so the
    /// category factor lands on the cold path rather than on every move.
    fn agg_sub(&mut self, mut target: Slot, node: Slot, sign: i32) {
        let ni = node as usize;
        let dc = sign * self.cnt[ni];
        let dx = sign as i64 * self.sx[ni];
        let dy = sign as i64 * self.sy[ni];
        let kk = self.categories;
        if kk == 0 {
            while target != NONE {
                let ti = target as usize;
                self.cnt[ti] += dc;
                self.sx[ti] += dx;
                self.sy[ti] += dy;
                target = self.par[ti];
            }
            return;
        }
        let nb = ni * kk;
        while target != NONE {
            let ti = target as usize;
            self.cnt[ti] += dc;
            self.sx[ti] += dx;
            self.sy[ti] += dy;
            let tb = ti * kk;
            for k in 0..kk {
                self.ccnt[tb + k] += sign * self.ccnt[nb + k];
                self.csx[tb + k] += sign as i64 * self.csx[nb + k];
                self.csy[tb + k] += sign as i64 * self.csy[nb + k];
            }
            target = self.par[ti];
        }
    }

    /// Children are kept sorted by level so a viewport query can stop early.
    fn add_child(&mut self, p: Slot, c: Slot) {
        self.par[c as usize] = p;
        let tzc = self.tz[c as usize];
        let mut prev = NONE;
        let mut cur = self.kid[p as usize];
        while cur != NONE && self.tz[cur as usize] < tzc {
            prev = cur;
            cur = self.sib[cur as usize];
        }
        self.sib[c as usize] = cur;
        self.psib[c as usize] = prev;
        if cur != NONE {
            self.psib[cur as usize] = c;
        }
        if prev == NONE {
            self.kid[p as usize] = c;
        } else {
            self.sib[prev as usize] = c;
        }
    }

    fn del_child(&mut self, c: Slot) {
        let ci = c as usize;
        let p = self.par[ci];
        if p == NONE {
            return;
        }
        let nx = self.sib[ci];
        let pv = self.psib[ci];
        if pv == NONE {
            self.kid[p as usize] = nx;
        } else {
            self.sib[pv as usize] = nx;
        }
        if nx != NONE {
            self.psib[nx as usize] = pv;
        }
        self.sib[ci] = NONE;
        self.psib[ci] = NONE;
        self.par[ci] = NONE;
    }

    // ------------------------------------------------------------ mutation --

    /// Place an already-positioned, already-aggregated slot into the hierarchy.
    ///
    /// `from` caps the sweep: when re-homing an orphan that did not move, its
    /// level can only get coarser -- a center covering it at a level `>= ` its old
    /// one would have violated separation *before* the deletion -- so levels finer
    /// than `old_level - 1` cannot produce a hit and are skipped.
    fn link(&mut self, s: Slot, from: Option<i32>) {
        let (x, y) = (self.qx[s as usize], self.qy[s as usize]);
        self.descend(x, y, s, from);
        let lvl = self.d_level;
        let p = self.d_parent;
        self.tz[s as usize] = lvl;
        self.grid_add(s);
        if p != NONE {
            self.add_child(p, s);
            self.agg_sub(p, s, 1);
        }
    }

    /// Insert a point, or move it if the id is already present.
    ///
    /// Returns the slot, which is an internal handle -- useful for debugging, not
    /// for storing.
    pub fn insert(&mut self, id: u64, lng: f64, lat: f64) -> Slot {
        let (x, y) = project(lng, lat);
        self.insert_projected(id, x, y, 0)
    }

    /// Insert with a category, for filtered queries. `cat` must be less than
    /// `Options::categories`.
    pub fn insert_with_category(&mut self, id: u64, lng: f64, lat: f64, cat: u32) -> Slot {
        let (x, y) = project(lng, lat);
        self.insert_projected(id, x, y, cat)
    }

    /// Insert using pre-projected fixed-point coordinates.
    ///
    /// Bypassing [`project`] is the honest way to compare two implementations:
    /// `sin`, `ln` and `atan` are not bit-identical across libms, so a harness
    /// that projects independently on each side can diverge for reasons that have
    /// nothing to do with the algorithm.
    pub fn insert_projected(&mut self, id: u64, x: i32, y: i32, cat: u32) -> Slot {
        if self.ids.contains_key(&id) {
            return self.move_to_projected(id, x, y);
        }
        assert!(
            self.categories == 0 || (cat as usize) < self.categories,
            "netcluster: category {} outside [0, {})",
            cat,
            self.categories
        );
        let s = self.alloc_slot();
        let si = s as usize;
        self.qx[si] = x;
        self.qy[si] = y;
        if self.categories > 0 {
            self.cat[si] = cat;
        }
        self.self_mass(s);
        self.ext[si] = id;
        self.ids.insert(id, s);
        self.link(s, None);
        self.stats.inserts += 1;
        s
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let s = match self.ids.get(&id) {
            Some(&s) => s,
            None => return false,
        };
        self.unlink(s);
        self.ids.remove(&id);
        self.free_slot(s);
        self.stats.removes += 1;
        true
    }

    /// Detach `s` from the hierarchy, re-homing its children. `s` keeps its own
    /// aggregates.
    fn unlink(&mut self, s: Slot) {
        let si = s as usize;
        self.grid_del(s); // must vanish before any child is re-homed
        let up = self.par[si];
        // 1. this point's own mass leaves the ancestor chain
        let k = if self.categories > 0 {
            Some(self.cat[si] as usize)
        } else {
            None
        };
        let (nx, ny) = (-(self.qx[si] as i64), -(self.qy[si] as i64));
        self.agg(up, -1, nx, ny, k);

        // 2. every child subtree is re-homed elsewhere
        let mut kids = std::mem::take(&mut self.kids);
        kids.clear();
        let mut c = self.kid[si];
        while c != NONE {
            kids.push(c);
            c = self.sib[c as usize];
        }
        let k_n = kids.len();

        // The list already arrives level-sorted, so only runs of equal level need
        // a canonical order; that is rare, so checking first keeps removal free of
        // the sort in the common case.
        let mut ties = false;
        for i in 1..k_n {
            if self.tz[kids[i] as usize] == self.tz[kids[i - 1] as usize] {
                ties = true;
                break;
            }
        }
        if ties {
            for i in 1..k_n {
                let v = kids[i];
                let vz = self.tz[v as usize];
                let vext = self.ext[v as usize];
                let mut j = i as isize - 1;
                while j >= 0 {
                    let kj = kids[j as usize];
                    let tzj = self.tz[kj as usize];
                    let after = tzj > vz
                        || (tzj == vz
                            && cmp_decimal(self.ext[kj as usize], vext) == Ordering::Greater);
                    if !after {
                        break;
                    }
                    kids[(j + 1) as usize] = kj;
                    j -= 1;
                }
                kids[(j + 1) as usize] = v;
            }
        }

        for &ch in kids.iter() {
            self.agg_sub(up, ch, -1);
            self.del_child(ch);
            let old_level = self.tz[ch as usize] as i32;
            self.grid_del(ch);
            self.link(ch, Some(old_level - 1));
            if (self.tz[ch as usize] as i32) < old_level {
                self.stats.promotions += 1;
            }
            self.stats.reparents += 1;
        }
        self.kids = kids;

        self.del_child(s);
        self.kid[si] = NONE;
        // s now carries exactly its own mass again
        self.self_mass(s);
        self.tz[si] = DEAD;
    }

    /// Move a point, or insert it if the id is unknown.
    pub fn move_to(&mut self, id: u64, lng: f64, lat: f64) -> Slot {
        let (x, y) = project(lng, lat);
        self.move_to_projected(id, x, y)
    }

    /// Move using pre-projected fixed-point coordinates.
    ///
    /// The fast path only touches the levels whose invariants the displacement
    /// actually breaks: a device that moves less than `r_maxZoom` does `O(depth)`
    /// integer adds and `O(log Δ)` hash probes and nothing else.
    pub fn move_to_projected(&mut self, id: u64, x: i32, y: i32) -> Slot {
        let s = match self.ids.get(&id) {
            Some(&s) => s,
            None => return self.insert_projected(id, x, y, 0),
        };
        let si = s as usize;
        let (ox, oy) = (self.qx[si], self.qy[si]);
        if x == ox && y == oy {
            return s;
        }
        self.stats.moves += 1;

        let t = self.tz[si] as i32;
        let mut ok = true;

        // (B) separation. p is a member of C_z for EVERY z >= t, so the move is
        // only legal if no center of any such C_z came within r_z. `covered_at_or_below`
        // computes exactly the finest level at which p is now covered; p may stay
        // put iff that level is coarser than t. Leaves carry no separation
        // constraint at all -- the common case for a dense fleet -- so they skip
        // the sweep entirely and cost one distance test.
        if t <= self.max_zoom as i32 && self.covered_at_or_below(x, y, t, s) >= 0 {
            ok = false;
        }
        // (C) still covered by our parent, with hysteresis?
        let p = self.par[si];
        if ok && p != NONE {
            let dx = (self.qx[p as usize] - x) as i64;
            let dy = (self.qy[p as usize] - y) as i64;
            ok = (dx * dx + dy * dy) as f64 <= self.hyst2[(t - 1) as usize];
        }
        // (C) do we still cover our own children?
        if ok {
            let mut c = self.kid[si];
            while c != NONE {
                let dx = (self.qx[c as usize] - x) as i64;
                let dy = (self.qy[c as usize] - y) as i64;
                let lim = self.hyst2[(self.tz[c as usize] - 1) as usize];
                if (dx * dx + dy * dy) as f64 > lim {
                    ok = false;
                    break;
                }
                c = self.sib[c as usize];
            }
        }

        if ok {
            self.grid_move(s, x, y);
            let k = if self.categories > 0 {
                Some(self.cat[si] as usize)
            } else {
                None
            };
            self.agg(s, 0, (x - ox) as i64, (y - oy) as i64, k);
            self.stats.moves_fast += 1;
            return s;
        }

        // slow path: local repair. Detach (children get re-homed), reposition, re-link.
        self.unlink(s);
        self.qx[si] = x;
        self.qy[si] = y;
        self.self_mass(s);
        self.link(s, None);
        self.stats.moves_rebuilt += 1;
        s
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[inline]
    pub fn max_zoom(&self) -> u8 {
        self.max_zoom
    }

    #[inline]
    pub fn min_zoom(&self) -> u8 {
        self.min_zoom
    }

    #[inline]
    pub fn categories(&self) -> usize {
        self.categories
    }

    pub fn options(&self) -> Options {
        Options {
            min_zoom: self.min_zoom,
            max_zoom: self.max_zoom,
            radius: self.radius,
            extent: self.extent,
            hysteresis: self.hysteresis,
            categories: self.categories,
        }
    }

    // --------------------------------------------------------------- query --
    //
    // Every method below takes &self. That is load-bearing for the server: it is
    // what lets N reader threads run concurrently against one writer behind an
    // RwLock, which is the whole reason to have this in Rust rather than JS.

    /// Aggregate of the cluster represented by center `s` at level `z`.
    ///
    /// With `cat >= 0` the same subtraction runs over that category's slice, so a
    /// filtered cluster costs exactly what an unfiltered one costs.
    pub(crate) fn cluster_at(&self, s: Slot, z: i32, cat: i32) -> (i32, i64, i64) {
        let kk = self.categories;
        if kk > 0 && cat >= 0 {
            let c0 = s as usize * kk + cat as usize;
            let mut c = self.ccnt[c0];
            let mut ax = self.csx[c0];
            let mut ay = self.csy[c0];
            let mut b = self.kid[s as usize];
            while b != NONE {
                if self.tz[b as usize] as i32 > z {
                    break;
                }
                let bi = b as usize * kk + cat as usize;
                c -= self.ccnt[bi];
                ax -= self.csx[bi];
                ay -= self.csy[bi];
                b = self.sib[b as usize];
            }
            return (c, ax, ay);
        }
        let si = s as usize;
        let mut c = self.cnt[si];
        let mut ax = self.sx[si];
        let mut ay = self.sy[si];
        let mut b = self.kid[si];
        while b != NONE {
            if self.tz[b as usize] as i32 > z {
                break; // sorted by level: everything after this is inside
            }
            let bi = b as usize;
            c -= self.cnt[bi];
            ax -= self.sx[bi];
            ay -= self.sy[bi];
            b = self.sib[bi];
        }
        (c, ax, ay)
    }

    /// How many points of `cat` sit anywhere under `s`.
    #[inline]
    pub(crate) fn subtree_count(&self, s: Slot, cat: i32) -> i32 {
        if self.categories > 0 && cat >= 0 {
            self.ccnt[s as usize * self.categories + cat as usize]
        } else {
            self.cnt[s as usize]
        }
    }

    /// The one member of category `cat` in cluster `(s, z)`.
    fn find_single(&self, s: Slot, z: i32, cat: i32) -> Slot {
        if self.cat[s as usize] as i32 == cat {
            return s;
        }
        let mut b = self.kid[s as usize];
        while b != NONE {
            if (self.tz[b as usize] as i32) > z && self.subtree_count(b, cat) > 0 {
                return self.find_single_in(b, cat);
            }
            b = self.sib[b as usize];
        }
        s
    }

    /// Same, once the whole subtree is known to be inside the cluster.
    fn find_single_in(&self, s: Slot, cat: i32) -> Slot {
        if self.cat[s as usize] as i32 == cat {
            return s;
        }
        let mut b = self.kid[s as usize];
        while b != NONE {
            if self.subtree_count(b, cat) > 0 {
                return self.find_single_in(b, cat);
            }
            b = self.sib[b as usize];
        }
        s
    }

    /// All clusters visible in `[min_lng, min_lat, max_lng, max_lat]` at `zoom`.
    ///
    /// Pass `category = -1` for no filter.
    pub fn get_clusters(&self, bbox: [f64; 4], zoom: f64, category: i32) -> Vec<Feature> {
        let (x0, y0) = project(bbox[0], bbox[3]);
        let (x1, y1) = project(bbox[2], bbox[1]);
        let z = (zoom.floor() as i32).clamp(self.min_zoom as i32, self.max_zoom as i32);
        self.get_clusters_projected(x0, y0, x1, y1, z, category)
    }

    /// Same query, in fixed-point world coordinates.
    ///
    /// Top-down traversal: the roots `C_0` are found through the level-0 grid (at
    /// most a couple hundred cells for the whole world), then we walk down through
    /// children whose level is `<= z`. A subtree rooted at `c` is pruned when
    /// `B(c, 2·r_tz[c])` -- which provably contains every descendant of `c` --
    /// misses the box. The number of visited nodes is therefore `O(K)` for `K`
    /// clusters returned, independent of `N`.
    pub fn get_clusters_projected(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        zoom: i32,
        category: i32,
    ) -> Vec<Feature> {
        let (x0, x1) = if x1 < x0 { (x1, x0) } else { (x0, x1) };
        let (y0, y1) = if y1 < y0 { (y1, y0) } else { (y0, y1) };
        let (x0, y0, x1, y1) = (x0 as f64, y0 as f64, x1 as f64, y1 as f64);
        let z = zoom.clamp(self.min_zoom as i32, self.max_zoom as i32);
        let cat = if self.categories > 0 { category } else { -1 };

        let mut out = Vec::new();
        let mut stack: Vec<Slot> = Vec::with_capacity(256);

        // roots: every center of C_0 whose subtree ball meets the box
        let cs = self.cs[0];
        let pad0 = 2.0 * self.r[0];
        let maxc = (PREC / cs).ceil() as i64;
        let cx0 = (((x0 - pad0) / cs).floor() as i64).max(0);
        let cx1 = (((x1 + pad0) / cs).floor() as i64).min(maxc);
        let cy0 = (((y0 - pad0) / cs).floor() as i64).max(0);
        let cy1 = (((y1 + pad0) / cs).floor() as i64).min(maxc);
        for cx in cx0..=cx1 {
            let base = cx as u64 * KEY_Y;
            for cy in cy0..=cy1 {
                let mut e = match self.grid.get(base + cy as u64) {
                    Some(e) => e,
                    None => continue,
                };
                loop {
                    stack.push(self.e_slot[e as usize]);
                    e = self.e_next[e as usize];
                    if e == NONE {
                        break;
                    }
                }
            }
        }

        while let Some(s) = stack.pop() {
            // a subtree holding none of the requested category cannot contribute
            if cat >= 0 && self.subtree_count(s, cat) == 0 {
                continue;
            }
            let si = s as usize;
            let pad = 2.0 * self.r[self.tz[si] as usize];
            let px = self.qx[si] as f64;
            let py = self.qy[si] as f64;
            if px < x0 - pad || px > x1 + pad || py < y0 - pad || py > y1 + pad {
                continue;
            }
            let (count, ax, ay) = self.cluster_at(s, z, cat);
            if count > 0 {
                // filtered clusters can be empty
                let mx = ax as f64 / count as f64;
                let my = ay as f64 / count as f64;
                if mx >= x0 && mx <= x1 && my >= y0 && my <= y1 {
                    // a filtered cluster of one is often a descendant, not the centre
                    let one = if count == 1 && cat >= 0 {
                        self.find_single(s, z, cat)
                    } else {
                        s
                    };
                    out.push(self.feature(one, z, count as u32, mx, my));
                }
            }
            let mut b = self.kid[si];
            while b != NONE {
                if self.tz[b as usize] as i32 > z {
                    break; // sorted: the rest are inside this cluster
                }
                stack.push(b);
                b = self.sib[b as usize];
            }
        }
        out
    }

    /// Clusters inside vector tile `(z, tx, ty)`, in tile-extent coordinates.
    ///
    /// The tile is padded by the cluster radius so a marker sitting on the seam is
    /// emitted by both neighbouring tiles rather than clipped.
    pub fn get_tile(&self, z: i32, tx: i64, ty: i64, category: i32) -> Vec<TileFeature> {
        if !(0..=30).contains(&z) {
            return Vec::new();
        }
        let z2 = (1u64 << z) as f64;
        let margin = self.radius / self.extent;
        let to_world = |t: f64| -> i32 {
            let v = t / z2 * PREC;
            if v < 0.0 {
                0
            } else if v > PREC {
                PREC as i32
            } else {
                v.round() as i32
            }
        };
        let bx0 = to_world(tx as f64 - margin);
        let bx1 = to_world(tx as f64 + 1.0 + margin);
        let by0 = to_world(ty as f64 - margin);
        let by1 = to_world(ty as f64 + 1.0 + margin);

        let feats = self.get_clusters_projected(bx0, by0, bx1, by1, z, category);
        let e = self.extent;
        feats
            .into_iter()
            .map(|f| {
                let (wx, wy) = crate::project::project(f.lng(), f.lat());
                let px = ((wx as f64 / PREC * z2 - tx as f64) * e).round() as i32;
                let py = ((wy as f64 / PREC * z2 - ty as f64) * e).round() as i32;
                match f {
                    Feature::Point { id, .. } => TileFeature {
                        x: px,
                        y: py,
                        count: 1,
                        id,
                        is_cluster: false,
                    },
                    Feature::Cluster {
                        cluster_id, count, ..
                    } => TileFeature {
                        x: px,
                        y: py,
                        count,
                        id: cluster_id,
                        is_cluster: true,
                    },
                }
            })
            .collect()
    }

    fn feature(&self, s: Slot, z: i32, count: u32, mx: f64, my: f64) -> Feature {
        let (lng, lat) = unproject(mx, my);
        if count == 1 {
            Feature::Point {
                id: self.ext[s as usize],
                lng,
                lat,
            }
        } else {
            Feature::Cluster {
                cluster_id: s as u64 * 32 + z as u64,
                count,
                lng,
                lat,
            }
        }
    }

    fn leaf_feature(&self, s: Slot) -> Feature {
        let si = s as usize;
        let (lng, lat) = unproject(self.qx[si] as f64, self.qy[si] as f64);
        Feature::Point {
            id: self.ext[si],
            lng,
            lat,
        }
    }

    /// Decode a cluster handle into `(slot, level)`, rejecting anything stale.
    fn decode(&self, cluster_id: u64) -> Result<(Slot, i32), ClusterIdError> {
        let slot = cluster_id / 32;
        let z = (cluster_id % 32) as i32;
        if slot >= self.n as u64 || z > self.max_zoom as i32 + 1 || self.tz[slot as usize] == DEAD {
            return Err(ClusterIdError { cluster_id });
        }
        Ok((slot as Slot, z))
    }

    /// The sub-clusters one expansion step below a cluster.
    pub fn get_children(&self, cluster_id: u64) -> Result<Vec<Feature>, ClusterIdError> {
        let (s, z) = self.decode(cluster_id)?;
        let nz = self.expansion_zoom(s, z);
        if nz > self.max_zoom as i32 {
            return Ok(vec![self.leaf_feature(s)]);
        }
        let mut res = Vec::new();
        let (c, ax, ay) = self.cluster_at(s, nz, -1);
        res.push(self.feature(s, nz, c as u32, ax as f64 / c as f64, ay as f64 / c as f64));
        let mut b = self.kid[s as usize];
        while b != NONE {
            let tzb = self.tz[b as usize] as i32;
            if tzb > nz {
                break;
            }
            if tzb > z {
                let (c, ax, ay) = self.cluster_at(b, nz, -1);
                res.push(self.feature(b, nz, c as u32, ax as f64 / c as f64, ay as f64 / c as f64));
            }
            b = self.sib[b as usize];
        }
        Ok(res)
    }

    /// Zoom at which a cluster first splits.
    pub fn get_cluster_expansion_zoom(&self, cluster_id: u64) -> Result<i32, ClusterIdError> {
        let (s, z) = self.decode(cluster_id)?;
        Ok(self.expansion_zoom(s, z))
    }

    fn expansion_zoom(&self, s: Slot, z: i32) -> i32 {
        let mut b = self.kid[s as usize];
        while b != NONE {
            let tzb = self.tz[b as usize] as i32;
            if tzb > z {
                return tzb;
            }
            b = self.sib[b as usize];
        }
        self.max_zoom as i32 + 1
    }

    /// Every individual point inside a cluster, paginated.
    pub fn get_leaves(
        &self,
        cluster_id: u64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Feature>, ClusterIdError> {
        let (s, z) = self.decode(cluster_id)?;
        let mut out = Vec::new();
        let mut skipped = 0usize;
        self.walk_leaves(s, z, limit, offset, &mut skipped, &mut out);
        Ok(out)
    }

    fn walk_leaves(
        &self,
        n: Slot,
        lvl: i32,
        limit: usize,
        offset: usize,
        skipped: &mut usize,
        out: &mut Vec<Feature>,
    ) {
        if out.len() >= limit {
            return;
        }
        if *skipped >= offset {
            out.push(self.leaf_feature(n));
        } else {
            *skipped += 1;
        }
        let mut b = self.kid[n as usize];
        while b != NONE {
            if (self.tz[b as usize] as i32) > lvl {
                self.walk_leaves(b, lvl, limit, offset, skipped, out);
                if out.len() >= limit {
                    return;
                }
            }
            b = self.sib[b as usize];
        }
    }

    /// The level-`z` representative of a point: the cluster it is drawn as.
    ///
    /// Returns the *external id* of the representative, or `None` if the point is
    /// not in the index.
    pub fn representative(&self, id: u64, z: i32) -> Option<u64> {
        let mut s = *self.ids.get(&id)?;
        while (self.tz[s as usize] as i32) > z {
            s = self.par[s as usize];
        }
        Some(self.ext[s as usize])
    }

    /// The cluster a device is drawn as at `zoom`.
    ///
    /// The direct answer to "my vehicle is somewhere on this map -- which marker
    /// is it inside?", which otherwise needs a viewport query and a search.
    pub fn cluster_of(&self, id: u64, zoom: i32) -> Option<Feature> {
        let z = zoom.clamp(self.min_zoom as i32, self.max_zoom as i32);
        let s = self.representative_slot(id, z)?;
        let (count, ax, ay) = self.cluster_at(s, z, -1);
        if count <= 0 {
            return None;
        }
        Some(self.feature(
            s,
            z,
            count as u32,
            ax as f64 / count as f64,
            ay as f64 / count as f64,
        ))
    }

    /// The level-`z` representative as an internal slot.
    pub fn representative_slot(&self, id: u64, z: i32) -> Option<Slot> {
        let mut s = *self.ids.get(&id)?;
        while (self.tz[s as usize] as i32) > z {
            s = self.par[s as usize];
        }
        Some(s)
    }

    /// Is a point with this id currently in the index?
    pub fn contains(&self, id: u64) -> bool {
        self.ids.contains_key(&id)
    }

    /// The category a point was inserted under, or `None` if it is not in the
    /// index. Always `Some(0)` when categories are disabled.
    pub fn category_of(&self, id: u64) -> Option<u32> {
        let s = *self.ids.get(&id)?;
        Some(if self.categories > 0 {
            self.cat[s as usize]
        } else {
            0
        })
    }

    /// Position of a point as longitude/latitude, or `None` if it is not in the
    /// index.
    pub fn position_of(&self, id: u64) -> Option<(f64, f64)> {
        let (x, y) = self.position(id)?;
        Some(unproject(x as f64, y as f64))
    }

    /// Position of a point in fixed-point world coordinates.
    pub fn position(&self, id: u64) -> Option<(i32, i32)> {
        let s = *self.ids.get(&id)?;
        Some((self.qx[s as usize], self.qy[s as usize]))
    }

    /// Rough resident size of the index in bytes.
    pub fn memory_bytes(&self) -> usize {
        let cap = self.cap();
        let per_slot = 2 * 4 + 2 * 8 + 4 + 4 * 4 + 1 + 8; // qx qy sx sy cnt par/kid/sib/psib tz ext
        let cat_bytes = if self.categories > 0 {
            cap * self.categories * (4 + 8 + 8) + cap * 4
        } else {
            0
        };
        cap * per_slot + cat_bytes + self.e_slot.len() * 8 + self.grid.bytes() + self.ids.len() * 40
    }

    /// `Σ_z |C_z|`: how many (center, level) pairs the grid holds.
    pub fn grid_entries(&self) -> usize {
        let mut free = 0usize;
        let mut e = self.e_free;
        while e != NONE {
            free += 1;
            e = self.e_next[e as usize];
        }
        self.e_n as usize - free
    }

    /// How many centers exist at each level, `0..=max_zoom`.
    ///
    /// Debug aid: walks every live slot, so it is `O(N)`.
    pub fn centers_per_level(&self) -> Vec<u32> {
        let mut out = vec![0u32; self.max_zoom as usize + 2];
        for s in 0..self.n as usize {
            let t = self.tz[s];
            if t == DEAD {
                continue;
            }
            for level in out.iter_mut().skip(t as usize) {
                *level += 1;
            }
        }
        out
    }
}

impl std::fmt::Debug for NetCluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately not derived: the parallel arrays are megabytes wide, and
        // what anyone printing an index actually wants is its shape.
        f.debug_struct("NetCluster")
            .field("points", &self.len())
            .field("slots", &self.n)
            .field("max_zoom", &self.max_zoom)
            .field("radius", &self.radius)
            .field("hysteresis", &self.hysteresis)
            .field("categories", &self.categories)
            .field("grid_entries", &self.grid_entries())
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

/// Lexicographic comparison of the *decimal renderings* of two ids.
///
/// This looks eccentric -- `10` sorts before `9` -- and it is deliberate. The
/// JavaScript implementation breaks exact distance ties with `String(a) < String(b)`
/// so that the tree is a function of the point set and the operation order alone.
/// Reproducing the rule exactly is what lets the Rust and JavaScript indexes be
/// compared device-for-device; for non-negative integers below 1e21, JavaScript's
/// `String` is the plain decimal expansion, so the two agree.
#[inline]
pub(crate) fn cmp_decimal(a: u64, b: u64) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let mut ba = [0u8; 20];
    let mut bb = [0u8; 20];
    let la = write_decimal(a, &mut ba);
    let lb = write_decimal(b, &mut bb);
    ba[..la].cmp(&bb[..lb])
}

#[inline]
fn write_decimal(mut n: u64, buf: &mut [u8; 20]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_tie_break_matches_javascript_string_ordering() {
        // The cases that make it eccentric, and therefore the ones that matter.
        assert_eq!(cmp_decimal(10, 9), Ordering::Less);
        assert_eq!(cmp_decimal(9, 10), Ordering::Greater);
        assert_eq!(cmp_decimal(1, 1), Ordering::Equal);
        assert_eq!(cmp_decimal(100, 11), Ordering::Less);
        assert_eq!(cmp_decimal(2, 19), Ordering::Greater);
        assert_eq!(cmp_decimal(0, 1), Ordering::Less);
        assert_eq!(cmp_decimal(u64::MAX, 1), Ordering::Greater);

        // Exhaustive cross-check against the actual rule, expressed directly.
        for a in 0..300u64 {
            for b in 0..300u64 {
                assert_eq!(
                    cmp_decimal(a, b),
                    a.to_string().cmp(&b.to_string()),
                    "{a} vs {b}"
                );
            }
        }
    }
}
