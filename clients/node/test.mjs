// Integration test. Spawns the real server on an ephemeral port and drives it
// through the client, so this exercises the wire format, not a mock.
//
//   cargo build --release --bin netcluster-server
//   node test.mjs
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { existsSync } from 'node:fs';
import assert from 'node:assert/strict';
import { NetClusterClient, NetClusterError, DEFAULT_MAX_BATCH } from './index.js';

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = process.env.NETCLUSTER_BIN ?? join(HERE, '../../target/release/netcluster-server');
const PORT = 8000 + Math.floor(Math.random() * 1000);
const URL = `http://127.0.0.1:${PORT}`;

if (!existsSync(BIN)) {
  console.error(`no server binary at ${BIN}\nbuild it first:  cargo build --release --bin netcluster-server`);
  process.exit(1);
}

const server = spawn(BIN, [], {
  env: { ...process.env, NETCLUSTER_ADDR: `127.0.0.1:${PORT}`, NETCLUSTER_SWEEP_SECONDS: '1' },
  stdio: ['ignore', 'ignore', 'pipe'],
});
let serverErr = '';
server.stderr.on('data', (d) => { serverErr += d; });
process.on('exit', () => server.kill());

const nc = new NetClusterClient({ url: URL, timeoutMs: 4000 });

// wait for the listener rather than guessing at a sleep
for (let i = 0; i < 100; i++) {
  try { await nc.health(); break; } catch { await new Promise((r) => setTimeout(r, 50)); }
  if (i === 99) { console.error(serverErr); throw new Error('server never came up'); }
}

let passed = 0;
async function test(name, fn) {
  try { await fn(); console.log(`  ok  ${name}`); passed++; }
  catch (e) { console.error(`  FAIL ${name}\n       ${e.message}`); process.exitCode = 1; }
}

await test('health', async () => {
  const h = await nc.health();
  assert.equal(h.status, 'ok');
  assert.equal(typeof h.uptime_ms, 'number');
});

const fleet = nc.collection('fleet');

await test('create a collection with named categories', async () => {
  const r = await fleet.create({ maxZoom: 16, ttlSeconds: 300, categories: ['idle', 'enroute', 'delivering'] });
  assert.equal(r.created, true);
  const again = await fleet.create({ maxZoom: 16, ttlSeconds: 300, categories: ['idle', 'enroute', 'delivering'] });
  assert.equal(again.created, false, 'creating twice with the same geometry must be idempotent');
});

await test('a different geometry on an existing name is a 409, not a silent no-op', async () => {
  await assert.rejects(
    () => fleet.create({ maxZoom: 12, categories: ['a'] }),
    (e) => e instanceof NetClusterError && e.status === 409
  );
});

await test('report and cluster', async () => {
  const r = await fleet.report([
    { id: 'truck-1', lng: -46.6333, lat: -23.5505, cat: 'delivering' },
    { id: 'truck-2', lng: -46.6340, lat: -23.5510, cat: 'delivering' },
    { id: 'truck-3', lng: -46.6350, lat: -23.5520, cat: 'idle' },
    { id: 'truck-4', lng: -43.1729, lat: -22.9068, cat: 'enroute' },
  ]);
  assert.equal(r.accepted, 4);
  assert.equal(r.devices, 4);
  const fc = await fleet.getClusters({ bbox: [-60, -35, -30, -10], zoom: 4 });
  assert.equal(fc.type, 'FeatureCollection');
  const total = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  assert.equal(total, 4, 'every device must be accounted for exactly once');
});

await test('filter by category label', async () => {
  const fc = await fleet.getClusters({ bbox: [-60, -35, -30, -10], zoom: 4, cat: 'delivering' });
  const total = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  assert.equal(total, 2);
});

await test('an unknown category is a clear 400, not an empty result', async () => {
  await assert.rejects(
    () => fleet.getClusters({ zoom: 4, cat: 'delivring' }),
    (e) => e instanceof NetClusterError && e.status === 400 && /unknown category/.test(e.message)
  );
});

await test('expand a cluster', async () => {
  const fc = await fleet.getClusters({ bbox: [-60, -35, -30, -10], zoom: 4 });
  const cluster = fc.features.find((f) => f.properties.cluster);
  assert.ok(cluster, 'expected at least one cluster');
  const kids = await fleet.getChildren(cluster.properties.cluster_id);
  assert.ok(kids.expansion_zoom > 4);
  const leaves = await fleet.getLeaves(cluster.properties.cluster_id, { limit: 100 });
  assert.equal(leaves.features.length, cluster.properties.point_count);
});

await test('find which marker a device is inside', async () => {
  const f = await fleet.deviceCluster('truck-1', 4);
  assert.ok(f.properties.cluster === true || f.properties.id === 'truck-1');
});

await test('vector tile comes back as MVT bytes', async () => {
  const buf = await fleet.getTile(0, 0, 0);
  assert.ok(buf instanceof Uint8Array, 'expected raw bytes');
  assert.ok(buf.length > 0);
  // protobuf field 3 (layers), wire type 2
  assert.equal(buf[0], (3 << 3) | 2, 'not a vector tile');
  const asJson = await fleet.getTile(0, 0, 0, { format: 'json' });
  assert.equal(asJson.type, 'FeatureCollection');
});

await test('remove a device', async () => {
  assert.deepEqual(await fleet.remove('truck-4'), { removed: true });
  assert.deepEqual(await fleet.remove('truck-4'), { removed: false });
  assert.equal((await fleet.stats()).devices, 3);
});

await test('reporter coalesces repeated reports for one device', async () => {
  const r = nc.reporter('fleet', { flushMs: 50_000, maxBatch: DEFAULT_MAX_BATCH });
  for (let i = 0; i < 10; i++) r.report({ id: 'truck-1', lng: -46.6 + i * 0.001, lat: -23.5 });
  r.report({ id: 'truck-9', lng: -46.7, lat: -23.6 });
  assert.equal(r.pending.size, 2, 'ten reports for one device must collapse to one');
  assert.equal(r.stats.coalesced, 9);
  await r.flush();
  assert.equal(r.stats.sent, 2);
  await r.close();
  // the last position wins
  const fc = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 16 });
  const t1 = fc.features.find((f) => f.properties.id === 'truck-1');
  assert.ok(Math.abs(t1.geometry.coordinates[0] - (-46.6 + 9 * 0.001)) < 1e-6);
});

await test('reporter flushes on its timer', async () => {
  const r = nc.reporter('fleet', { flushMs: 60 });
  r.report({ id: 'timer-1', lng: 10, lat: 10 });
  await new Promise((res) => setTimeout(res, 250));
  assert.equal(r.stats.sent, 1);
  await r.close();
});

await test('reporter requeues on failure instead of dropping positions', async () => {
  const broken = new NetClusterClient({ url: 'http://127.0.0.1:1', timeoutMs: 300, retries: 0 });
  const errs = [];
  const r = broken.reporter('fleet', { flushMs: 60_000, onError: (e) => errs.push(e) });
  r.report({ id: 'x', lng: 1, lat: 1 });
  await r.flush();
  assert.equal(errs.length, 1);
  assert.equal(r.pending.size, 1, 'a failed flush must not lose the position');
  clearInterval(r._timer);
});

await test('expiry drops devices that stop reporting', async () => {
  const tmp = nc.collection('ephemeral');
  await tmp.create({ ttlSeconds: 1 });
  await tmp.report([{ id: 'ghost', lng: 1, lat: 1 }, { id: 'alive', lng: 2, lat: 2 }]);
  assert.equal((await tmp.stats()).devices, 2);
  for (let i = 0; i < 12; i++) {
    await new Promise((res) => setTimeout(res, 250));
    await tmp.report([{ id: 'alive', lng: 2, lat: 2 }]);
    if ((await tmp.stats()).devices === 1) break;
  }
  assert.equal((await tmp.stats()).devices, 1, 'the silent device should have expired');
  await tmp.drop();
});

await test('the index still passes its own invariant check', async () => {
  const v = await fleet.verify();
  assert.equal(v.ok, true, v.violation);
});

await test('forViewer pins reads to one replica', async () => {
  const many = new NetClusterClient({ urls: [URL, 'http://b:8080', 'http://c:8080'] });
  const a = many.forViewer('session-abc');
  const b = many.forViewer('session-abc');
  assert.equal(a._readBase(), b._readBase(), 'the same key must pin to the same replica');
  assert.equal(a._readBase(), a._readBase(), 'a pinned view must not round-robin');
  assert.ok(many.urls.length === 3 && a.urls.length === 3);
});

await test('a 4xx is not retried', async () => {
  let calls = 0;
  const counting = new NetClusterClient({
    url: URL, retries: 5,
    fetch: (...a) => { calls++; return globalThis.fetch(...a); },
  });
  await assert.rejects(() => counting.getClusters('fleet', { zoom: 4, cat: 'nope' }));
  assert.equal(calls, 1, `retried a 400 ${calls} times`);
});

// The example's last section asserts it called every public method. Running it
// here is what keeps that assertion meaningful: add a method to the client and
// forget to demonstrate it, and this fails.
await test('example.mjs exercises every public method', async () => {
  const { status, stdout, stderr } = spawnSync(
    process.execPath,
    [join(HERE, 'example.mjs')],
    { env: { ...process.env, NETCLUSTER_URL: URL }, encoding: 'utf8' }
  );
  const out = stdout + stderr;
  if (status !== 0) {
    const tail = out.trim().split('\n').slice(-6).join('\n       ');
    throw new Error(`example.mjs exited ${status}\n       ${tail}`);
  }
  const m = out.match(/(\d+) of (\d+) public methods exercised/);
  assert.ok(m, 'example.mjs printed no coverage line');
  assert.equal(m[1], m[2], `example.mjs covered ${m[1]} of ${m[2]} methods`);
});

server.kill();
console.log(`\n${passed} passed`);
