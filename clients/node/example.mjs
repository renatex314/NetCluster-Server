// A simulated fleet reporting into a running server.
//
//   cargo run --release -p netcluster-server      # in another terminal
//   node example.mjs
import { NetClusterClient } from './index.js';

const nc = new NetClusterClient({ url: process.env.NETCLUSTER_URL ?? 'http://localhost:8080' });
const fleet = nc.collection('example-fleet');

await fleet.create({ maxZoom: 16, ttlSeconds: 120, categories: ['idle', 'enroute', 'delivering'] });

const HUBS = [[-46.63, -23.55], [-43.17, -22.90], [-47.88, -15.79], [-38.52, -3.73]];
const N = 20_000;
const vehicles = Array.from({ length: N }, (_, i) => {
  const h = HUBS[i % HUBS.length];
  const a = Math.random() * Math.PI * 2;
  return {
    id: `v${i}`,
    lng: h[0] + (Math.random() - 0.5) * 0.8,
    lat: h[1] + (Math.random() - 0.5) * 0.8,
    cat: ['idle', 'enroute', 'delivering'][i % 3],
    hx: Math.cos(a) * 0.0004,
    hy: Math.sin(a) * 0.0004,
  };
});

// The reporter batches and coalesces; you just hand it every report you receive.
const reporter = fleet.reporter({ flushMs: 500, onError: (e) => console.error('flush:', e.message) });

console.log(`reporting ${N.toLocaleString()} vehicles, ctrl-c to stop`);
const tick = setInterval(() => {
  for (const v of vehicles) {
    if (Math.random() < 0.02) {
      const a = Math.random() * Math.PI * 2;
      v.hx = Math.cos(a) * 0.0004;
      v.hy = Math.sin(a) * 0.0004;
    }
    v.lng += v.hx;
    v.lat += v.hy;
    reporter.report({ id: v.id, lng: v.lng, lat: v.lat, cat: v.cat });
  }
}, 200);

setInterval(async () => {
  const t0 = performance.now();
  const fc = await fleet.getClusters({ bbox: [-60, -35, -30, 0], zoom: 6 });
  const ms = (performance.now() - t0).toFixed(1);
  const s = await fleet.stats();
  const shown = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  console.log(
    `${s.devices.toLocaleString()} devices | ${fc.features.length} markers covering ` +
    `${shown.toLocaleString()} vehicles | query ${ms} ms | ` +
    `${(s.memory_bytes / 1e6).toFixed(0)} MB | fast-path ${s.moves_fast_pct.toFixed(1)}% | ` +
    `coalesced ${reporter.stats.coalesced.toLocaleString()} of ${reporter.stats.queued.toLocaleString()}`
  );
}, 2000);

process.on('SIGINT', async () => {
  clearInterval(tick);
  await reporter.close();
  console.log('\nstopped');
  process.exit(0);
});
