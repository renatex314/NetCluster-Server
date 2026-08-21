// Integration test. Spawns the real server on an ephemeral port and drives it
// through the client, so this exercises the wire format, not a mock.
//
//   cargo build --release --bin netcluster-server
//   node test.mjs
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
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

const DATA = join(tmpdir(), `netcluster-test-${process.pid}-${Date.now()}`);
const server = spawn(BIN, [], {
  env: {
    ...process.env,
    NETCLUSTER_ADDR: `127.0.0.1:${PORT}`,
    NETCLUSTER_SWEEP_SECONDS: '1',
    // Persistence on, so the snapshot path is covered rather than skipped.
    NETCLUSTER_DATA_DIR: DATA,
    NETCLUSTER_SNAPSHOT_SECONDS: '3600',
  },
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

await test('has() and getDevice()', async () => {
  assert.equal(await fleet.has('truck-1'), true);
  assert.equal(await fleet.has('never-reported'), false);

  const d = await fleet.getDevice('truck-1');
  assert.equal(d.id, 'truck-1');
  assert.ok(Math.abs(d.lng - -46.6333) < 1e-6, `lng ${d.lng}`);
  assert.ok(Math.abs(d.lat - -23.5505) < 1e-6, `lat ${d.lat}`);
  assert.equal(d.cat, 'delivering');
  assert.equal(d.cat_index, 2);
  assert.ok(d.age_ms >= 0 && d.age_ms < 60_000, `age_ms ${d.age_ms}`);
  assert.equal(await fleet.getDevice('never-reported'), null);
});

// A device that is gone is not registered; a collection that is gone is an error.
// Collapsing the two would turn a typo in a collection name into an empty map.
await test('has() distinguishes an unknown device from an unknown collection', async () => {
  await fleet.report([{ id: 'temp-1', lng: 1, lat: 1 }]);
  assert.equal(await fleet.has('temp-1'), true);
  await fleet.remove('temp-1');
  assert.equal(await fleet.has('temp-1'), false, 'a removed device is not registered');

  await assert.rejects(
    () => nc.has('no-such-collection', 'truck-1'),
    (e) =>
      e instanceof NetClusterError &&
      e.status === 404 &&
      e.body.code === 'no_such_collection'
  );
});

await test('an expired device stops being registered', async () => {
  const tmp = nc.collection('reg-expiry');
  await tmp.create({ ttlSeconds: 1 });
  await tmp.report([{ id: 'ghost', lng: 1, lat: 1 }]);
  assert.equal(await tmp.has('ghost'), true);
  let gone = false;
  for (let i = 0; i < 12; i++) {
    await new Promise((r) => setTimeout(r, 250));
    if (!(await tmp.has('ghost'))) { gone = true; break; }
  }
  assert.ok(gone, 'a device that stopped reporting is still registered');
  assert.equal(await tmp.getDevice('ghost'), null);
  await tmp.drop();
});

await test('free-form properties round-trip through the API', async () => {
  await fleet.report([{
    id: 'truck-1',
    lng: -46.6333, lat: -23.5505, cat: 'delivering',
    props: { plate: 'ABC-1234', driver: 'Ana', battery: 87, tags: ['cold'], nested: { a: 1 } },
  }]);
  const d = await fleet.getDevice('truck-1');
  assert.equal(d.props.plate, 'ABC-1234');
  assert.equal(d.props.battery, 87);
  assert.deepEqual(d.props.tags, ['cold']);
  assert.equal(d.props.nested.a, 1, 'nested structure was flattened');

  const fc = await fleet.getClusters({ bbox: [-47, -24, -46, -23], zoom: 16 });
  const f = fc.features.find((x) => x.id === 'truck-1');
  assert.equal(f.properties.plate, 'ABC-1234', 'properties missing from the query path');

  // a cluster has none: forty vehicles do not share a battery level
  const low = await fleet.getClusters({ bbox: [-60, -35, -30, -10], zoom: 2 });
  const cluster = low.features.find((x) => x.properties.cluster);
  assert.ok(cluster, 'expected a cluster');
  assert.equal(cluster.properties.battery, undefined);
});

await test('a position report does not erase properties', async () => {
  for (let i = 0; i < 10; i++) {
    await fleet.report([{ id: 'truck-1', lng: -46.6333 + i * 0.0001, lat: -23.5505 }]);
  }
  const d = await fleet.getDevice('truck-1');
  assert.equal(d.props.plate, 'ABC-1234', '10 position reports erased the properties');

  await fleet.report([{ id: 'truck-1', lng: -46.6333, lat: -23.5505, props: {} }]);
  assert.deepEqual((await fleet.getDevice('truck-1')).props, {}, 'an empty object should clear');
  await fleet.report([{ id: 'truck-1', lng: -46.6333, lat: -23.5505,
                        props: { plate: 'ABC-1234' }, cat: 'delivering' }]);
});

// Coalescing keeps the newest report, and a position report carries no props --
// so without care, reporting properties then a position before the next flush
// would discard them before they were ever sent.
await test('the reporter does not lose properties when coalescing', async () => {
  const r = nc.reporter('fleet', { flushMs: 60_000 });
  r.report({ id: 'coalesce-1', lng: 1, lat: 1, props: { plate: 'KEEP-ME' } });
  r.report({ id: 'coalesce-1', lng: 2, lat: 2 });
  r.report({ id: 'coalesce-1', lng: 3, lat: 3 });
  assert.equal(r.pending.size, 1);
  assert.deepEqual(r.pending.get('coalesce-1').props, { plate: 'KEEP-ME' });
  await r.flush();
  const d = await fleet.getDevice('coalesce-1');
  assert.equal(d.props.plate, 'KEEP-ME', 'coalescing dropped the properties');
  assert.ok(Math.abs(d.lng - 3) < 1e-6, `the newest position should win, got ${d.lng}`);

  // but an explicit props on a newer report still wins
  r.report({ id: 'coalesce-1', lng: 4, lat: 4, props: { plate: 'NEWER' } });
  r.report({ id: 'coalesce-1', lng: 5, lat: 5 });
  await r.flush();
  assert.equal((await fleet.getDevice('coalesce-1')).props.plate, 'NEWER');
  await r.close();
  await fleet.remove('coalesce-1');
});

await test('oversized and malformed properties are rejected', async () => {
  const tmp = nc.collection('props-cap');
  await tmp.create({ maxPropsBytes: 64 });
  await tmp.report([{ id: 'a', lng: 1, lat: 1, props: { a: 'short' } }]);
  await assert.rejects(
    () => tmp.report([{ id: 'b', lng: 1, lat: 1, props: { a: 'x'.repeat(200) } }]),
    (e) => e instanceof NetClusterError && e.status === 400 && /limit is 64/.test(e.message)
  );
  assert.equal(await tmp.has('b'), false);
  await tmp.drop();
});

// A stray attribute at the top level is a mistake; silently discarding it means
// finding out weeks later that nothing was ever stored.
await test('unknown top-level fields are rejected, not silently dropped', async () => {
  const res = await fetch(`${URL}/v1/collections/fleet/positions`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify([{ id: 'x', lng: 1, lat: 1, plate: 'ABC-1234' }]),
  });
  assert.equal(res.status, 422, `expected a rejection, got ${res.status}`);
  assert.equal(await fleet.has('x'), false);
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
  // `feature.id` rather than `properties.id`: once a device has props, its
  // properties object is the props verbatim. The id is on the feature, which is
  // where GeoJSON puts it.
  const t1 = fc.features.find((f) => f.id === 'truck-1');
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

await test('snapshot() writes a file that survives a restart', async () => {
  const { existsSync, readdirSync } = await import('node:fs');
  const r = await fleet.snapshot();
  assert.ok(r.bytes > 0, 'snapshot reported no bytes');
  assert.equal(r.snapshot, 'fleet');
  assert.ok(existsSync(DATA), 'no data directory');
  assert.ok(readdirSync(DATA).some((f) => f.endsWith('.ncs')), 'no snapshot file');

  const st = await fleet.stats();
  assert.ok(st.last_snapshot_ms > 0, 'stats did not record the snapshot');
  assert.equal(st.snapshot_failures, 0);
  assert.equal(st.last_snapshot_bytes, r.bytes);

  assert.equal((await nc.health()).persistence, true);
});

// The whole point: a fresh process reading the same directory comes back with the
// devices AND the geometry, so filters keep working after a restart.
await test('a second process restores from the snapshot', async () => {
  const before = await fleet.stats();
  await fleet.snapshot();

  const PORT2 = PORT + 1;
  const second = spawn(BIN, [], {
    env: { ...process.env, NETCLUSTER_ADDR: `127.0.0.1:${PORT2}`, NETCLUSTER_DATA_DIR: DATA },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  try {
    const nc2 = new NetClusterClient({ url: `http://127.0.0.1:${PORT2}` });
    for (let i = 0; i < 100; i++) {
      try { await nc2.health(); break; } catch { await new Promise((r) => setTimeout(r, 50)); }
    }
    const after = await nc2.stats('fleet');
    assert.equal(after.devices, before.devices, 'device count changed across the restart');
    assert.equal(after.restored, before.devices, 'devices were not counted as restored');
    assert.deepEqual(after.categories, before.categories, 'the geometry did not come back');
    assert.equal(after.ttl_seconds, before.ttl_seconds);

    // a filtered query still works, which is what the geometry restore buys
    const fc = await nc2.getClusters('fleet', { zoom: 8, cat: 'delivering' });
    assert.ok(fc.features.length > 0, 'the restored collection lost its categories');

    const d = await nc2.getDevice('fleet', 'truck-2');
    assert.ok(d, 'truck-2 did not survive');
    const orig = await fleet.getDevice('truck-2');
    assert.equal(d.lng, orig.lng, 'position drifted across the restart');
    assert.equal(d.lat, orig.lat);
  } finally {
    second.kill();
  }
});

await test('dropping a collection removes its snapshot', async () => {
  const { readdirSync } = await import('node:fs');
  const tmp = nc.collection('will-be-dropped');
  await tmp.create({ ttlSeconds: 0 });
  await tmp.report([{ id: 'x', lng: 1, lat: 1 }]);
  await tmp.snapshot();
  const before = readdirSync(DATA).length;
  const res = await tmp.drop();
  assert.equal(res.snapshot_removed, true, 'the snapshot file was left behind');
  assert.equal(readdirSync(DATA).length, before - 1, 'file count did not drop');
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
try {
  const { rmSync } = await import('node:fs');
  rmSync(DATA, { recursive: true, force: true });
} catch { /* best effort */ }
console.log(`\n${passed} passed`);
