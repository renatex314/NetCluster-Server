#!/usr/bin/env node
// Repeatable one-process HTTP contention check. No installed service is touched.
// node scripts/bench-contention.mjs <server-binary> [label]
// N=50000 WRITERS=8 READERS=8 ROUNDS=4 TIMEOUT_MS=5000
import { spawn } from 'node:child_process';
import { setTimeout as sleep } from 'node:timers/promises';
import { performance } from 'node:perf_hooks';
import { resolve } from 'node:path';
import assert from 'node:assert/strict';
const N = Number(process.env.N ?? 50_000);
const batch = 500;
const writers = Number(process.env.WRITERS ?? 8);
const readers = Number(process.env.READERS ?? 8);
const rounds = Number(process.env.ROUNDS ?? 4);
const timeoutMs = Number(process.env.TIMEOUT_MS ?? 5000);
const port = Number(process.env.PORT ?? 19138);
const base = 'http://127.0.0.1:' + port;
const binary = resolve(process.argv[2] ?? 'target/release/netcluster-server');
const server = spawn(binary, [], {
  windowsHide: true,
  env: { ...process.env, NETCLUSTER_ADDR: '127.0.0.1:' + port,
    TOKIO_WORKER_THREADS: '2', NETCLUSTER_MAX_BLOCKING: '2',
    NETCLUSTER_DATA_DIR: '', NETCLUSTER_SWEEP_SECONDS: '10',
    NETCLUSTER_AUTO_CREATE: '0' },
  stdio: ['ignore', 'ignore', 'pipe'],
});
let errors = '';
server.stderr.on('data', d => { errors = (errors + d).slice(-8000); });
server.on('error', e => { errors += e.message; });
process.on('exit', () => server.kill());
const metrics = {};
const expected = new Map();
async function request(kind, path, body, measured = true) {
  const start = performance.now();
  let status = 'timeout', parsed;
  try {
    const response = await fetch(base + path, {
      method: body === undefined ? 'GET' : 'POST',
      headers: { 'content-type': 'application/json' }, body,
      signal: AbortSignal.timeout(timeoutMs),
    });
    status = response.status;
    // Include receiving the complete response, not merely the headers.
    const text = await response.text();
    if (kind === 'write' && response.ok) parsed = JSON.parse(text);
  } catch (e) { status = e.name === 'TimeoutError' ? 'timeout' : 'network-error'; }
  if (measured) {
    const m = metrics[kind] ??= { latencies: [], statuses: {} };
    m.latencies.push(performance.now() - start);
    m.statuses[status] = (m.statuses[status] ?? 0) + 1;
  }
  return { status, parsed };
}
function point(i, round) {
  const a = (i * 2.39996) % (Math.PI * 2), r = Math.sqrt(i / N) * .6;
  return { id: 'v' + i, lng: -46.63 + Math.cos(a) * r + round * .0001,
    lat: -23.55 + Math.sin(a) * r * .8, props: { plate: 'ABC-' + i, round } };
}
try {
  let ready = false;
  for (let i = 0; i < 100; i++) {
    if (server.exitCode !== null) throw Error(errors);
    try { if ((await fetch(base + '/healthz')).ok) { ready = true; break; } } catch {}
    await sleep(50);
  }
  assert(ready, 'server failed to start: ' + errors);
  const create = await fetch(base + '/v1/collections/fleet', {
    method: 'PUT', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ max_zoom: 16, ttl_seconds: 0, text: ['plate'] }),
  });
  assert(create.ok, await create.text());
  for (let i = 0; i < N; i += batch) {
    const points = Array.from({ length: Math.min(batch, N - i) }, (_, j) => point(i + j, 0));
    const result = await request('write', '/v1/collections/fleet/positions', JSON.stringify(points), false);
    assert.equal(result.status, 200, 'preload failed');
    assert.equal(result.parsed.accepted, points.length);
    points.forEach(p => expected.set(p.id, p));
  }
  const started = performance.now();
  let writing = true;
  const readTasks = Array.from({ length: readers }, async (_, reader) => {
    let n = 0;
    while (writing) {
      // A mix of expensive wide queries and ordinary clustered viewports.
      const zoom = (reader + n++) % 4 === 0 ? 20 : 10;
      await request('read', '/v1/collections/fleet/clusters?zoom=' + zoom);
    }
  });
  const health = (async () => {
    while (writing) { await request('health', '/healthz'); await sleep(25); }
  })();
  await Promise.all(Array.from({ length: writers }, async (_, writer) => {
    for (let round = 1; round <= rounds; round++) {
      for (let chunk = writer; chunk * batch < N; chunk += writers) {
        const i = chunk * batch;
        const points = Array.from({ length: Math.min(batch, N - i) }, (_, j) => point(i + j, round));
        const r = await request('write', '/v1/collections/fleet/positions', JSON.stringify(points));
        if (r.status === 200) {
          assert.equal(r.parsed.accepted, points.length);
          points.forEach(p => expected.set(p.id, p));
        }
      }
    }
  }));
  writing = false;
  await Promise.all([...readTasks, health]);
  const elapsedMs = performance.now() - started;
  const response = await fetch(base + '/v1/collections/fleet/clusters?zoom=20');
  const fc = await response.json();
  assert.equal(fc.features.length, N, 'missing or duplicate devices after contention');
  let mismatches = 0;
  for (const f of fc.features) {
    const p = expected.get(f.id);
    if (!p || Math.abs(f.geometry.coordinates[0] - p.lng) > 1e-6 ||
        Math.abs(f.geometry.coordinates[1] - p.lat) > 1e-6 ||
        f.properties.round !== p.props.round || f.properties.plate !== p.props.plate) mismatches++;
  }
  const stats = await (await fetch(base + '/v1/collections/fleet')).json();
  const summary = {};
  for (const [kind, m] of Object.entries(metrics)) {
    m.latencies.sort((a, b) => a - b);
    const pct = p => +m.latencies[Math.min(m.latencies.length - 1, Math.floor(m.latencies.length * p))].toFixed(1);
    summary[kind] = { requests: m.latencies.length, statuses: m.statuses,
      p50_ms: pct(.5), p95_ms: pct(.95), p99_ms: pct(.99), max_ms: pct(1) };
  }
  console.log(JSON.stringify({ label: process.argv[3], N, writers, readers, rounds,
    runtime_workers: 2, timeoutMs, elapsed_ms: Math.round(elapsedMs),
    final_devices: stats.devices, mismatches, repairs: stats.repairs ?? null, results: summary }, null, 2));
  assert.equal(mismatches, 0, 'accepted updates did not match final map positions/properties');
} finally {
  server.kill();
  await new Promise(resolve => { if (server.exitCode !== null) resolve(); else server.once('exit', resolve); });
}
