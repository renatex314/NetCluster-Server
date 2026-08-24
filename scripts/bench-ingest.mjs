#!/usr/bin/env node
// Ingest throughput, both body formats, through real HTTP with real JSON parsing.
//
//   cargo build --release -p netcluster-server
//   node scripts/bench-ingest.mjs
//
// The point is the comparison, not the absolute number: GeoJSON carries more
// bytes and more structure per point than the compact form, and this says how
// much more that costs. Everything downstream of parsing -- interning, the
// index, the lock -- is identical, so the gap is the format and nothing else.
import { spawn } from 'node:child_process';
import { setTimeout as sleep } from 'node:timers/promises';

const N = Number(process.env.N || 100_000);
const BATCH = Number(process.env.BATCH || 1000);
const PORT = Number(process.env.PORT || 8109);
const BIN = process.env.BIN || 'target/release/netcluster-server';
const BASE = `http://127.0.0.1:${PORT}`;
// ONLY=compact restricts the run, so the same script can measure a build that
// predates GeoJSON support and the comparison stays like-for-like.
const ONLY = process.env.ONLY ? new RegExp(process.env.ONLY, 'i') : null;

const pts = [];
for (let i = 0; i < N; i++) {
  // a plausible metro spread, so the tree has real shape
  const a = (i * 2.39996) % (Math.PI * 2), r = Math.sqrt(i / N) * 0.6;
  pts.push([-46.63 + Math.cos(a) * r, -23.55 + Math.sin(a) * r * 0.8]);
}

const server = spawn(BIN, [], {
  env: { ...process.env, NETCLUSTER_ADDR: `127.0.0.1:${PORT}`, NETCLUSTER_AUTO_CREATE: '0',
         NETCLUSTER_DATA_DIR: '', NETCLUSTER_SWEEP_SECONDS: '0' },
  stdio: ['ignore', 'ignore', 'inherit'],
});
process.on('exit', () => server.kill('SIGTERM'));

for (let i = 0; i < 100; i++) {
  try { if ((await fetch(`${BASE}/healthz`)).ok) break; } catch { /* not up yet */ }
  await sleep(50);
}

const put = (name, body) =>
  fetch(`${BASE}/v1/collections/${name}`, {
    method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  });

async function run(name, makeBatch, { categories = [] } = {}) {
  await fetch(`${BASE}/v1/collections/${name}`, { method: 'DELETE' });
  await put(name, { max_zoom: 16, radius: 40, ttl_seconds: 0, categories });

  // Serialise every batch up front. JSON.stringify in the client is not what is
  // being measured, and at 100k points it is comparable to the server's work.
  const bodies = [];
  for (let i = 0; i < N; i += BATCH) bodies.push(JSON.stringify(makeBatch(i, Math.min(i + BATCH, N))));

  const t0 = process.hrtime.bigint();
  let bytes = 0;
  for (const body of bodies) {
    bytes += body.length;
    const r = await fetch(`${BASE}/v1/collections/${name}/positions`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body,
    });
    if (!r.ok) throw new Error(`${name}: ${r.status} ${await r.text()}`);
  }
  const secs = Number(process.hrtime.bigint() - t0) / 1e9;

  const stats = await (await fetch(`${BASE}/v1/collections/${name}`)).json();
  if (stats.devices !== N) throw new Error(`${name}: indexed ${stats.devices}, expected ${N}`);
  return { secs, rate: N / secs, bytes };
}

const compact = (a, b) => {
  const out = new Array(b - a);
  for (let i = a; i < b; i++) out[i - a] = { id: `v${i}`, lng: pts[i][0], lat: pts[i][1] };
  return out;
};
const compactProps = (a, b) => {
  const out = new Array(b - a);
  for (let i = a; i < b; i++) out[i - a] = { id: `v${i}`, lng: pts[i][0], lat: pts[i][1], props: { plate: `ABC-${i}` } };
  return out;
};
const feature = (i, props) => ({
  type: 'Feature', id: `v${i}`, properties: props,
  geometry: { type: 'Point', coordinates: [pts[i][0], pts[i][1]] },
});
const geo = (a, b) => {
  const features = new Array(b - a);
  for (let i = a; i < b; i++) features[i - a] = feature(i, null);
  return { type: 'FeatureCollection', features };
};
const geoProps = (a, b) => {
  const features = new Array(b - a);
  for (let i = a; i < b; i++) features[i - a] = feature(i, { plate: `ABC-${i}` });
  return { type: 'FeatureCollection', features };
};
const geoCat = (a, b) => {
  const features = new Array(b - a);
  for (let i = a; i < b; i++) features[i - a] = feature(i, { plate: `ABC-${i}`, cat: i % 4 });
  return { type: 'FeatureCollection', features };
};

const rows = [];
const add = async (label, name, fn, opts) => {
  if (ONLY && !ONLY.test(label)) return;
  await run(name, fn, opts);                     // warm: first pass grows the arena
  const r = await run(name, fn, opts);           // measured: every device now moves
  rows.push({ body: label, 'reports/s': Math.round(r.rate).toLocaleString('en-US'),
              'MB posted': (r.bytes / 1e6).toFixed(1), 'µs/report': (1e6 * r.secs / N).toFixed(2) });
};

await add('compact  [{id,lng,lat}]', 'c1', compact);
await add('compact  + props', 'c2', compactProps);
await add('GeoJSON  FeatureCollection', 'g1', geo);
await add('GeoJSON  + properties', 'g2', geoProps);
await add('GeoJSON  + properties + category', 'g3', geoCat, { categories: ['a', 'b', 'c', 'd'] });

const cols = Object.keys(rows[0]);
const w = cols.map((c) => Math.max(c.length, ...rows.map((r) => String(r[c]).length)));
const line = (cs) => '  ' + cs.map((c, i) => String(c).padStart(w[i])).join('  ');
console.log(`\n  ingest through HTTP, N = ${N.toLocaleString('en-US')} devices, batches of ${BATCH}\n`);
console.log(line(cols));
console.log('  ' + w.map((x) => '-'.repeat(x)).join('  '));
for (const r of rows) console.log(line(cols.map((c) => r[c])));
console.log();
server.kill('SIGTERM');
