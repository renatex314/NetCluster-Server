// Regression tests with an injected transport; see server tests for HTTP coverage.
import assert from 'node:assert/strict';
import test from 'node:test';
import { NetClusterClient } from './index.js';

test('duplicate device IDs in one implicit-version batch keep the last position', async () => {
  const server = transport();
  const client = new NetClusterClient({ fetch: server.fetch });
  await client.report('fleet', [{id:'v',lng:1,lat:1}, {id:'v',lng:20,lat:20}]);
  assert.equal(server.devices.get('v').lng, 20);
});

test('GeoJSON stamps all chunks before the first request and preserves explicit versions', async () => {
  const bodies = [];
  const client = new NetClusterClient({ fetch: async (_url, opts) => {
    bodies.push(JSON.parse(opts.body));
    return response({ accepted: JSON.parse(opts.body).features.length, stale: 0 });
  }});
  const feature = {type:'Feature',id:'v',updatedAt:200,
    properties:{plate:'NEW'},geometry:{type:'Point',coordinates:[20,20]}};
  await client.reportGeoJSON('fleet', [feature], {maxBatch:1});
  assert.equal(bodies[0].features[0].updated_at_ms, 200);
  assert.equal(bodies[0].features[0].updatedAt, undefined);
  assert.equal(feature.updated_at_ms, undefined, 'must not mutate the caller');
});

test('caller mutation after enqueue cannot change the queued report', async () => {
  const server = transport();
  const client = new NetClusterClient({ fetch:server.fetch });
  const reporter = client.reporter('fleet', {flushMs:60_000});
  const point = {id:'v',lng:20,lat:20,props:{plate:'NEW'},updatedAt:200};
  reporter.report(point);
  point.props.plate = 'MUTATED';
  point.lng = 1;
  await reporter.close();
  assert.equal(server.devices.get('v').props.plate, 'NEW');
  assert.equal(server.devices.get('v').lng, 20);
});

test('503 retries with the exact payload; explicit cancellation never retries', async () => {
  const sent = [];
  const client = new NetClusterClient({retries:1, fetch:async (_url, opts) => {
    sent.push(opts.body);
    if (sent.length === 1) return {ok:false,status:503,headers:new Headers({'retry-after':'0'}),
      text:async () => JSON.stringify({code:'overloaded',error:'busy'})};
    return response({accepted:1,stale:0});
  }});
  await client.report('fleet', {id:'v',lng:1,lat:1,updatedAt:200});
  assert.equal(sent.length, 2);
  assert.equal(sent[0], sent[1]);
  const controller = new AbortController();
  controller.abort(new Error('cancelled by caller'));
  await assert.rejects(client._req('http://test', '/healthz', {signal:controller.signal}), /cancelled/);
  assert.equal(sent.length, 2);
});

test('invalid batch sizes reject instead of looping forever', async () => {
  const client = new NetClusterClient({ fetch: async () => { throw Error('must not send'); }});
  for (const maxBatch of [0, -1, NaN, 1.5]) {
    await assert.rejects(client.report('fleet', [{id:'v',lng:1,lat:1}], {maxBatch}), /maxBatch/);
  }
});

const response = (body) => ({ ok: true, status: 200, json: async () => body });
function deferred() {
  let resolve;
  const promise = new Promise((r) => { resolve = r; });
  return { promise, resolve };
}
function transport() {
  const devices = new Map();
  const sent = [];
  const apply = (body) => {
    let accepted = 0, stale = 0;
    for (const point of JSON.parse(body)) {
      const previous = devices.get(point.id);
      if (previous && point.updated_at_ms < previous.updated_at_ms) {
        stale++;
      } else {
        devices.set(point.id, point);
        accepted++;
      }
    }
    return response({ accepted, stale, devices: devices.size });
  };
  return {
    devices, sent, apply,
    fetch: async (_url, options) => { sent.push(options.body); return apply(options.body); },
  };
}
async function withClock(fn) {
  const original = Date.now;
  let now = 1000;
  Date.now = () => now;
  try { await fn((value) => { now = value; }); }
  finally { Date.now = original; }
}

test('transport retry preserves the exact serialized body', async () => {
  const server = transport();
  let calls = 0;
  const client = new NetClusterClient({ retries: 1, fetch: async (_url, opts) => {
    server.sent.push(opts.body);
    if (++calls === 1) throw new Error('lost response');
    return server.apply(opts.body);
  } });
  await client.report('fleet', { id: 'v', lng: 1, lat: 1, updatedAt: 200 });
  assert.equal(server.sent[0], server.sent[1]);
});

test('explicit source versions reject an older report', async () => {
  const server = transport();
  const client = new NetClusterClient({ fetch: server.fetch });
  await client.report('fleet', { id: 'v', lng: 20, lat: 20, updatedAt: 200 });
  const old = await client.report('fleet', { id: 'v', lng: 10, lat: 10, updatedAt: 100 });
  assert.equal(old.stale, 1);
  assert.equal(server.devices.get('v').lng, 20);
});

test('a delayed backfill chunk must not overwrite a newer live update', async () => {
  await withClock(async (setTime) => {
    const server = transport();
    const started = deferred(), release = deferred();
    const client = new NetClusterClient({ fetch: async (_url, options) => {
      server.sent.push(options.body);
      if (JSON.parse(options.body)[0].id === 'first-chunk') {
        started.resolve();
        await release.promise;
      }
      return server.apply(options.body);
    } });
    const backfill = client.report('fleet', [
      { id: 'first-chunk', lng: 0, lat: 0 },
      { id: 'vehicle', lng: 1, lat: 1 },
    ], { maxBatch: 1 });
    await started.promise;
    setTime(2000);
    await client.report('fleet', { id: 'vehicle', lng: 20, lat: 20 });
    setTime(3000);
    release.resolve();
    await backfill;
    assert.equal(server.devices.get('vehicle').lng, 20,
      `old backfill replaced live position; wire=${JSON.stringify(server.sent.map(JSON.parse))}`);
  });
});

test('a reporter requeue must retain the original source timestamp', async () => {
  await withClock(async (setTime) => {
    const server = transport();
    let fail = true;
    const client = new NetClusterClient({ retries: 0, fetch: async (_url, options) => {
      server.sent.push(options.body);
      const result = server.apply(options.body);
      if (fail) { fail = false; throw new Error('response lost after server accepted write'); }
      return result;
    } });
    const reporter = client.reporter('fleet', { flushMs: 3600000 });
    try {
      reporter.report({ id: 'vehicle', lng: 1, lat: 1 });
      await assert.rejects(reporter.flush());
      setTime(2000);
      await client.report('fleet', { id: 'vehicle', lng: 20, lat: 20 });
      setTime(3000);
      await reporter.flush();
      assert.equal(server.devices.get('vehicle').lng, 20,
        `requeued old report received a newer timestamp; wire=${JSON.stringify(server.sent.map(JSON.parse))}`);
    } finally { await reporter.close(); }
  });
});

test('two reports in one millisecond must not regress when delivery is reversed', async () => {
  await withClock(async () => {
    const server = transport();
    const started = deferred(), release = deferred();
    const client = new NetClusterClient({ fetch: async (_url, options) => {
      server.sent.push(options.body);
      if (JSON.parse(options.body)[0].lng === 1) {
        started.resolve();
        await release.promise;
      }
      return server.apply(options.body);
    } });
    const older = client.report('fleet', { id: 'vehicle', lng: 1, lat: 1 });
    await started.promise;
    await client.report('fleet', { id: 'vehicle', lng: 20, lat: 20 });
    release.resolve();
    await older;
    assert.equal(server.devices.get('vehicle').lng, 20,
      `equal default timestamps allow reverse delivery to win; wire=${JSON.stringify(server.sent.map(JSON.parse))}`);
  });
});

test('coalescing must retain the newest explicit source version', async () => {
  const server = transport();
  const client = new NetClusterClient({ fetch: server.fetch });
  const reporter = client.reporter('fleet', { flushMs: 3600000 });
  try {
    reporter.report({ id: 'vehicle', lng: 20, lat: 20, updatedAt: 200 });
    reporter.report({ id: 'vehicle', lng: 1, lat: 1, updatedAt: 100 });
    await reporter.flush();
    assert.equal(server.devices.get('vehicle').lng, 20);
  } finally { await reporter.close(); }
});

test('an unexplained short acknowledgement must be surfaced as an error', async () => {
  const client = new NetClusterClient({ fetch: async () => response({ accepted: 0, stale: 0 }) });
  await assert.rejects(client.report('fleet', { id: 'vehicle', lng: 1, lat: 1 }));
});
