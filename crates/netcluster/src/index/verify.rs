//! Brute-force verification of every structural invariant.
//!
//! This is the safety net the whole design rests on. The claim "the invariants
//! are repaired locally on every update, forever, without a rebuild" is only
//! worth anything if something checks it independently of the code that does the
//! repairing -- so nothing here consults the index's own bookkeeping. Subtree
//! sums are recomputed by walking the tree, separation is checked pairwise, and
//! grid membership is recomputed from raw coordinates.

use super::*;
use std::collections::{HashMap, HashSet};

/// Summary of a successful verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verification {
    pub points: usize,
    /// Points that are a center at some level, i.e. `tz <= max_zoom`.
    pub centers: usize,
    /// Points that are never a center: they only ever appear inside a cluster.
    pub leaves: usize,
    /// `Σ_z |C_z|` -- how many (center, level) pairs the grid holds.
    pub grid_listings: usize,
    pub max_depth: usize,
}

macro_rules! bail {
    ($($arg:tt)*) => { return Err(format!($($arg)*)) };
}

impl NetCluster {
    /// Check every invariant, returning a summary or the first violation found.
    ///
    /// Intended for tests, fuzzing, and staging assertions. **`O(N²)`** -- the
    /// separation check compares every pair of centers, deliberately, because
    /// using the grid to check the grid would prove nothing. Do not call it on a
    /// large index on a hot path.
    pub fn verify(&self) -> Result<Verification, String> {
        let leaf = self.max_zoom as i32 + 1;
        let mut live: Vec<Slot> = self.ids.values().copied().collect();
        live.sort_unstable();
        let live_set: HashSet<Slot> = live.iter().copied().collect();

        if live.len() != self.ids.len() {
            bail!("id map holds duplicate slots");
        }

        // ---- 1. tree shape: levels strictly increase downward, roots are level 0,
        //         and every child is covered by its parent (with hysteresis slack)
        for &s in &live {
            let si = s as usize;
            let t = self.tz[si] as i32;
            if !(0..=leaf).contains(&t) {
                bail!("slot {s} has level {t}");
            }
            let p = self.par[si];
            if t == 0 {
                if p != NONE {
                    bail!("root {s} has parent {p}");
                }
                continue;
            }
            if p == NONE {
                bail!("slot {s} at level {t} has no parent");
            }
            if !live_set.contains(&p) {
                bail!("slot {s} parent {p} is dead");
            }
            if self.tz[p as usize] as i32 >= t {
                bail!("level inversion: {s}@{t} under {p}@{}", self.tz[p as usize]);
            }
            let dx = (self.qx[p as usize] - self.qx[si]) as i64;
            let dy = (self.qy[p as usize] - self.qy[si]) as i64;
            let lim = self.r[(t - 1) as usize] * (1.0 + self.hysteresis);
            let d2 = (dx * dx + dy * dy) as f64;
            if d2 > lim * lim + 1e-6 {
                bail!(
                    "covering violated: d({s},{p})={:.1} > {:.1} at level {}",
                    d2.sqrt(),
                    lim,
                    t - 1
                );
            }
        }

        // ---- 2. child lists agree with parent pointers, and are level-sorted
        let mut seen_child: HashSet<Slot> = HashSet::new();
        let mut max_depth = 0usize;
        for &s in &live {
            let mut prev = NONE;
            let mut last_lvl = -1i32;
            let mut c = self.kid[s as usize];
            while c != NONE {
                if !seen_child.insert(c) {
                    bail!("node {c} appears in two child lists");
                }
                if self.par[c as usize] != s {
                    bail!("child {c} of {s} has parent {}", self.par[c as usize]);
                }
                if self.psib[c as usize] != prev {
                    bail!("psib broken at {c}");
                }
                let tzc = self.tz[c as usize] as i32;
                if tzc < last_lvl {
                    bail!("child list of {s} is not sorted by level");
                }
                last_lvl = tzc;
                prev = c;
                c = self.sib[c as usize];
            }
        }
        for &s in &live {
            if self.par[s as usize] != NONE && !seen_child.contains(&s) {
                bail!("node {s} is not in its parent's child list");
            }
        }

        // ---- 3. SEPARATION: any two centers of C_z are more than r_z apart
        let centers: Vec<Slot> = live
            .iter()
            .copied()
            .filter(|&s| (self.tz[s as usize] as i32) <= self.max_zoom as i32)
            .collect();
        for i in 0..centers.len() {
            for j in (i + 1)..centers.len() {
                let (a, b) = (centers[i], centers[j]);
                // both are members of C_z for this z
                let z = (self.tz[a as usize]).max(self.tz[b as usize]) as usize;
                let dx = (self.qx[a as usize] - self.qx[b as usize]) as i64;
                let dy = (self.qy[a as usize] - self.qy[b as usize]) as i64;
                let d2 = (dx * dx + dy * dy) as f64;
                if d2 <= self.r[z] * self.r[z] - 1e-6 {
                    bail!(
                        "separation violated at level {z}: d({a}@{},{b}@{})={:.1} <= {:.1}",
                        self.tz[a as usize],
                        self.tz[b as usize],
                        d2.sqrt(),
                        self.r[z]
                    );
                }
            }
        }

        // ---- 4. aggregates equal brute-force subtree sums
        let mut sub: HashMap<Slot, (i32, i64, i64)> = HashMap::with_capacity(live.len());
        for &s in &live {
            if self.par[s as usize] == NONE {
                let mut path = HashSet::new();
                let (_, depth) = self.subtree_sums(s, &mut sub, &mut path, 0)?;
                max_depth = max_depth.max(depth);
            }
        }
        if sub.len() != live.len() {
            bail!(
                "forest covers {} of {} nodes (orphans or cycles)",
                sub.len(),
                live.len()
            );
        }
        for &s in &live {
            let si = s as usize;
            let t = sub[&s];
            if self.cnt[si] != t.0 {
                bail!("cnt[{s}]={} want {}", self.cnt[si], t.0);
            }
            if self.sx[si] != t.1 {
                bail!("sx[{s}] drift {}", self.sx[si] - t.1);
            }
            if self.sy[si] != t.2 {
                bail!("sy[{s}] drift {}", self.sy[si] - t.2);
            }
        }

        // ---- 4b. category slices sum to the totals they slice
        let k = self.categories;
        if k > 0 {
            for &s in &live {
                let si = s as usize;
                let (mut c, mut ax, mut ay) = (0i32, 0i64, 0i64);
                for i in 0..k {
                    c += self.ccnt[si * k + i];
                    ax += self.csx[si * k + i];
                    ay += self.csy[si * k + i];
                }
                if c != self.cnt[si] || ax != self.sx[si] || ay != self.sy[si] {
                    bail!(
                        "category slices of {s} sum to ({c},{ax},{ay}), totals are ({},{},{})",
                        self.cnt[si],
                        self.sx[si],
                        self.sy[si]
                    );
                }
            }
        }

        // ---- 5. the grid holds exactly the centers: a center of C_z must be listed
        //         in every level from tz down to max_zoom, once each, in the right cell
        let mut seen: HashSet<(Slot, i32)> = HashSet::new();
        for (key, head) in self.grid.iter() {
            let z = (key / KEY_X) as i32;
            let mut e = head;
            if e == NONE {
                bail!("empty cell left in the grid at level {z}");
            }
            loop {
                let s = self.e_slot[e as usize];
                if !live_set.contains(&s) {
                    bail!("dead slot {s} listed in grid level {z}");
                }
                if self.tz[s as usize] as i32 > z {
                    bail!("slot {s}@{} listed at level {z}", self.tz[s as usize]);
                }
                let cs = self.cs[z as usize];
                let cx = (self.qx[s as usize] as f64 / cs).floor() as i64;
                let cy = (self.qy[s as usize] as f64 / cs).floor() as i64;
                if Self::key(z, cx, cy) != key {
                    bail!("slot {s} is in the wrong cell at level {z}");
                }
                if !seen.insert((s, z)) {
                    bail!("slot {s} listed twice at level {z}");
                }
                e = self.e_next[e as usize];
                if e == NONE {
                    break;
                }
            }
        }
        let mut expect = 0usize;
        for &s in &centers {
            for z in (self.tz[s as usize] as i32)..=(self.max_zoom as i32) {
                if !seen.contains(&(s, z)) {
                    bail!(
                        "center {s}@{} missing from grid level {z}",
                        self.tz[s as usize]
                    );
                }
                expect += 1;
            }
        }
        if seen.len() != expect {
            bail!("grid holds {} listings, expected {expect}", seen.len());
        }
        if self.grid_entries() != expect {
            bail!(
                "entry pool leak: {} live entries vs {expect} listings",
                self.grid_entries()
            );
        }

        // ---- 6. every level's clustering is a partition of the live set, the
        //         radius bound holds, and cluster_at agrees with the partition
        for z in 0..=(self.max_zoom as i32) {
            let mut groups: HashMap<Slot, i32> = HashMap::new();
            for &s in &live {
                let mut a = s;
                let mut hops = 0;
                while (self.tz[a as usize] as i32) > z {
                    a = self.par[a as usize];
                    hops += 1;
                    if hops > leaf + 2 {
                        bail!("representative walk from {s} at z={z} does not terminate");
                    }
                }
                let dx = (self.qx[a as usize] - self.qx[s as usize]) as i64;
                let dy = (self.qy[a as usize] - self.qy[s as usize]) as i64;
                let lim = 2.0 * (1.0 + self.hysteresis) * self.r[z as usize];
                let d2 = (dx * dx + dy * dy) as f64;
                if d2 > lim * lim + 1e-6 {
                    bail!(
                        "radius bound broken at z={z}: {:.1} > {:.1}",
                        d2.sqrt(),
                        lim
                    );
                }
                *groups.entry(a).or_insert(0) += 1;
            }
            let mut total = 0i32;
            for (&a, &n) in &groups {
                let (c, _, _) = self.cluster_at(a, z, -1);
                if c != n {
                    bail!("cluster aggregate at z={z} for {a}: {c} != {n}");
                }
                total += n;
            }
            if total as usize != live.len() {
                bail!("level {z} partition covers {total}/{}", live.len());
            }
        }

        let leaves = live.len() - centers.len();
        Ok(Verification {
            points: live.len(),
            centers: centers.len(),
            leaves,
            grid_listings: expect,
            max_depth,
        })
    }

    fn subtree_sums(
        &self,
        s: Slot,
        sub: &mut HashMap<Slot, (i32, i64, i64)>,
        path: &mut HashSet<Slot>,
        depth: usize,
    ) -> Result<((i32, i64, i64), usize), String> {
        if !path.insert(s) {
            bail!("cycle at {s}");
        }
        if sub.contains_key(&s) {
            bail!("node {s} reached twice");
        }
        let si = s as usize;
        let mut acc = (1i32, self.qx[si] as i64, self.qy[si] as i64);
        let mut deepest = depth;
        let mut b = self.kid[si];
        while b != NONE {
            let (t, d) = self.subtree_sums(b, sub, path, depth + 1)?;
            acc.0 += t.0;
            acc.1 += t.1;
            acc.2 += t.2;
            deepest = deepest.max(d);
            b = self.sib[b as usize];
        }
        sub.insert(s, acc);
        path.remove(&s);
        Ok((acc, deepest))
    }
}

#[cfg(test)]
mod tests {
    use crate::{NetCluster, Options};

    /// A checker that never fails proves nothing. Each case below breaks exactly
    /// one invariant by writing to the internals directly, and asserts that
    /// `verify` notices -- and notices *that* one.
    fn built() -> NetCluster {
        let mut nc = NetCluster::new(Options::default());
        let mut seed = 7u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f64 / 4_294_967_296.0
        };
        for i in 0..200u64 {
            nc.insert(i, rnd() * 360.0 - 180.0, rnd() * 140.0 - 70.0);
        }
        nc.verify().expect("baseline must be clean");
        nc
    }

    fn a_live_slot(nc: &NetCluster) -> u32 {
        *nc.ids.values().next().unwrap()
    }

    #[test]
    fn catches_a_corrupted_aggregate() {
        let mut nc = built();
        let s = a_live_slot(&nc) as usize;
        nc.cnt[s] += 1;
        let e = nc.verify().unwrap_err();
        assert!(e.contains("cnt["), "wrong diagnosis: {e}");
    }

    #[test]
    fn catches_a_drifted_coordinate_sum() {
        let mut nc = built();
        let s = a_live_slot(&nc) as usize;
        nc.sx[s] += 1;
        let e = nc.verify().unwrap_err();
        assert!(e.contains("sx["), "wrong diagnosis: {e}");
    }

    /// Displace a point without telling the index: its parent no longer covers it
    /// and its grid cell is now wrong.
    #[test]
    fn catches_a_covering_violation() {
        let mut nc = built();
        let s = nc
            .ids
            .values()
            .copied()
            .find(|&s| nc.par[s as usize] != crate::NONE)
            .expect("some point must have a parent");
        nc.qx[s as usize] = nc.qx[s as usize].wrapping_add(200_000_000).abs();
        let e = nc.verify().unwrap_err();
        assert!(
            e.contains("covering violated") || e.contains("wrong cell") || e.contains("sx["),
            "wrong diagnosis: {e}"
        );
    }

    #[test]
    fn catches_a_severed_child_list() {
        let mut nc = built();
        let s = nc
            .ids
            .values()
            .copied()
            .find(|&s| nc.kid[s as usize] != crate::NONE)
            .expect("some point must have a child");
        nc.kid[s as usize] = crate::NONE;
        let e = nc.verify().unwrap_err();
        assert!(
            e.contains("not in its parent's child list") || e.contains("forest covers"),
            "wrong diagnosis: {e}"
        );
    }

    #[test]
    fn catches_two_centers_on_top_of_each_other() {
        let mut nc = built();
        let mut it = nc.ids.values().copied();
        let a = it.next().unwrap();
        let b = it.next().unwrap();
        nc.qx[b as usize] = nc.qx[a as usize];
        nc.qy[b as usize] = nc.qy[a as usize];
        let e = nc.verify().unwrap_err();
        assert!(
            e.contains("separation violated")
                || e.contains("covering violated")
                || e.contains("wrong cell")
                || e.contains("sx["),
            "wrong diagnosis: {e}"
        );
    }

    #[test]
    fn catches_a_missing_grid_listing() {
        let mut nc = built();
        let s = nc
            .ids
            .values()
            .copied()
            .find(|&s| (nc.tz[s as usize] as i32) <= nc.max_zoom as i32)
            .unwrap();
        let z = nc.tz[s as usize] as i32;
        let cs = nc.cs[z as usize];
        let cx = (nc.qx[s as usize] as f64 / cs).floor() as i64;
        let cy = (nc.qy[s as usize] as f64 / cs).floor() as i64;
        nc.grid_del_at(s, z, cx, cy);
        let e = nc.verify().unwrap_err();
        assert!(
            e.contains("missing from grid") || e.contains("leak"),
            "wrong diagnosis: {e}"
        );
    }
}
