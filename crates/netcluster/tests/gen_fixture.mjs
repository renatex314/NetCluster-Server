#!/usr/bin/env node
// Generate the differential-test fixture from the ORIGINAL JavaScript
// implementation. The Rust test replays the recorded operations and must produce
// the same clustering, device for device, at every zoom, at every checkpoint.
//
//   NETCLUSTER_JS=/path/to/netcluster-js node gen_fixture.mjs > fixtures/differential.txt
//
// Two decisions make the comparison meaningful:
//
//  1. Coordinates are recorded ALREADY PROJECTED. sin/ln/atan are not bit-identical
//     across libm implementations, so a harness that projects independently on each
//     side would eventually diverge for reasons that have nothing to do with the
//     algorithm. Recording the integers pins the input exactly.
//
//  2. The scenario includes exact-duplicate coordinates. When every distance is
//     zero, the id tie-break is the ONLY thing that determines the tree -- so if
//     the two implementations disagree about that rule, this fixture catches it.

// A static `import` needs a literal specifier, so resolve the path at runtime.
const JS_ROOT = process.env.NETCLUSTER_JS ?? '/Users/renatoacorte/Documents/projetos/new_algorithm';
const { NetCluster, project } = await import(JS_ROOT + '/src/netcluster.js');

function rng(seed) {
  let s = seed >>> 0;
  return () => { s ^= s << 13; s >>>= 0; s ^= s >> 17; s ^= s << 5; s >>>= 0; return s / 4294967296; };
}

const FNV_OFFSET = 14695981039346656037n, FNV_PRIME = 1099511628211n, M64 = (1n << 64n) - 1n;
function fnv64(str) {
  let h = FNV_OFFSET;
  for (let i = 0; i < str.length; i++) { h ^= BigInt(str.charCodeAt(i)); h = (h * FNV_PRIME) & M64; }
  return h.toString();
}

const CITIES = [[-46.63,-23.55],[2.35,48.85],[-74.0,40.71],[139.69,35.68],[116.4,39.9],
                [-43.17,-22.9],[13.4,52.52],[151.2,-33.87],[55.27,25.2],[-99.13,19.43]];

const out = [];

// The full level-by-level partition, as the map from every live device to the
// device that represents it. This is the strongest statement of "the two indexes
// agree" -- not cluster counts, not centroids, but who is grouped with whom.
function checkpoint(idx, live) {
  const ids = [...live].sort((a, b) => a - b);
  const parts = [`c ${ids.length}`];
  for (let z = 0; z <= idx.maxZoom; z++) {
    const reps = [];
    const distinct = new Set();
    for (const id of ids) {
      const slot = idx.representative(id, z);
      const rep = idx.ext[slot];
      reps.push(`${id}>${rep}`);
      distinct.add(rep);
    }
    parts.push(`${z}:${distinct.size}:${fnv64(reps.join(','))}`);
  }
  out.push(parts.join(' '));
}

function scenario(name, seed, opts, N, steps, world, withDupes) {
  const rnd = rng(seed);
  const idx = new NetCluster(opts);
  const live = new Set();
  const K = opts.categories ?? 0;
  out.push(`# scenario ${name}`);
  out.push(`opts ${opts.minZoom ?? 0} ${opts.maxZoom} ${opts.radius ?? 40} ${opts.extent ?? 512} ` +
           `${opts.hysteresis ?? 0.25} ${K}`);

  const pick = () => {
    if (rnd() < 0.7) {
      const c = world[Math.floor(rnd() * world.length)];
      return [c[0] + (rnd() - 0.5) * 0.4, c[1] + (rnd() - 0.5) * 0.4];
    }
    return [rnd() * 360 - 180, rnd() * 140 - 70];
  };
  const catOf = () => K ? Math.floor(rnd() * K) : 0;

  const doInsert = (id, lng, lat) => {
    const c = catOf();
    const [x, y] = project(lng, lat);
    idx.insert(id, lng, lat, K ? { category: c } : undefined);
    live.add(id);
    out.push(`i ${id} ${x} ${y} ${c}`);
  };
  const doMove = (id, lng, lat) => {
    const [x, y] = project(lng, lat);
    idx.moveTo(id, lng, lat);
    out.push(`m ${id} ${x} ${y}`);
  };

  const pos = new Map();
  for (let i = 0; i < N; i++) { const p = pick(); doInsert(i, p[0], p[1]); pos.set(i, p); }
  checkpoint(idx, live);

  let nextId = N;
  for (let step = 0; step < steps; step++) {
    const u = rnd();
    const keys = [...pos.keys()];
    if (u < 0.55 && keys.length) {
      const id = keys[Math.floor(rnd() * keys.length)];
      const p = pos.get(id);
      const q = rnd() < 0.85
        ? [p[0] + (rnd() - 0.5) * 0.02, p[1] + (rnd() - 0.5) * 0.02]
        : pick();
      doMove(id, q[0], q[1]); pos.set(id, q);
    } else if (u < 0.78) {
      const p = pick(); doInsert(nextId, p[0], p[1]); pos.set(nextId, p); nextId++;
    } else if (keys.length) {
      const id = keys[Math.floor(rnd() * keys.length)];
      idx.remove(id); pos.delete(id); live.delete(id);
      out.push(`r ${id}`);
    }
    if (step % 250 === 0) checkpoint(idx, live);
  }
  checkpoint(idx, live);

  if (withDupes) {
    // Every pairwise distance is zero here, so the id tie-break rule is the only
    // thing that determines the resulting tree.
    for (let i = 0; i < 60; i++) doInsert(2000000 + i, 10, 10);
    checkpoint(idx, live);
    for (let i = 0; i < 30; i++) { idx.remove(2000000 + i); live.delete(2000000 + i); out.push(`r ${2000000 + i}`); }
    checkpoint(idx, live);
  }
  out.push('end');
}

scenario('mixed-z16',   12345, { maxZoom: 16 },                          400, 3000, CITIES, true);
scenario('shallow-z6',  999,   { maxZoom: 6 },                           300, 1500, CITIES, true);
scenario('nohyst',      777,   { maxZoom: 14, hysteresis: 0 },           300, 1500, CITIES, true);
scenario('bighyst',     31337, { maxZoom: 12, hysteresis: 1.0 },         250, 1200, CITIES, true);
scenario('one-city',    4242,  { maxZoom: 16 },                          400, 2000, [[-46.63,-23.55]], true);
scenario('categorised', 5150,  { maxZoom: 16, categories: 5 },           400, 2000, CITIES, true);
scenario('tight-radius',8181,  { maxZoom: 16, radius: 100, extent: 512 }, 300, 1500, CITIES, true);

process.stdout.write(out.join('\n') + '\n');
