// The CLI, driven as a subprocess against a real server.
//
//   cargo build --release --bin netcluster-server
//   node test-cli.mjs
//
// Shelling out rather than importing: exit codes and stdout are the interface a
// script depends on, and neither is observable from inside the process.
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { existsSync, writeFileSync, rmSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import assert from 'node:assert/strict';

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = process.env.NETCLUSTER_BIN ?? join(HERE, '../../target/release/netcluster-server');
const PORT = 9000 + Math.floor(Math.random() * 900);
const URL = `http://127.0.0.1:${PORT}`;
const DATA = join(tmpdir(), `netcluster-cli-${process.pid}-${Date.now()}`);

if (!existsSync(BIN)) {
  console.error(`no server binary at ${BIN}\nbuild it:  cargo build --release --bin netcluster-server`);
  process.exit(1);
}

const server = spawn(BIN, [], {
  env: { ...process.env, NETCLUSTER_ADDR: `127.0.0.1:${PORT}`, NETCLUSTER_DATA_DIR: DATA },
  stdio: ['ignore', 'ignore', 'ignore'],
});
process.on('exit', () => server.kill());

/** Run the CLI. NO_COLOR so assertions match plain text. */
function cli(args, opts = {}) {
  return spawnSync(process.execPath, [join(HERE, 'cli.js'), ...args], {
    encoding: 'utf8',
    input: opts.input,
    env: { ...process.env, NETCLUSTER_URL: URL, NO_COLOR: '1' },
  });
}

for (let i = 0; i < 100; i++) {
  if (cli(['health']).status === 0) break;
  await new Promise((r) => setTimeout(r, 50));
  if (i === 99) throw new Error('server never came up');
}

let passed = 0;
function test(name, fn) {
  try { fn(); console.log(`  ok  ${name}`); passed++; }
  catch (e) { console.error(`  FAIL ${name}\n       ${e.message}`); process.exitCode = 1; }
}

test('health', () => {
  const r = cli(['health']);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /status\s+ok/);
});

test('--json is machine readable', () => {
  const r = cli(['health', '--json']);
  assert.equal(r.status, 0);
  assert.equal(JSON.parse(r.stdout).status, 'ok');
});

test('create is idempotent and reports which happened', () => {
  const a = cli(['create', 'fleet', '--categories', 'idle,enroute,delivering', '--ttl', '600']);
  assert.equal(a.status, 0, a.stderr);
  assert.match(a.stdout, /created/);
  const b = cli(['create', 'fleet', '--categories', 'idle,enroute,delivering', '--ttl', '600']);
  assert.equal(b.status, 0);
  assert.match(b.stdout, /exists/);
});

test('a different geometry is refused, not silently ignored', () => {
  const r = cli(['create', 'fleet', '--max-zoom', '10']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /already exists/);
});

test('seed and collections', () => {
  const s = cli(['seed', 'fleet', '--count', '5000', '--categories', 'idle,enroute,delivering']);
  assert.equal(s.status, 0, s.stderr);
  assert.match(s.stdout, /seeded 5,000/);
  const c = cli(['collections', '--json']);
  const found = JSON.parse(c.stdout).find((x) => x.name === 'fleet');
  assert.equal(found.devices, 5000);
});

test('report carries category and props', () => {
  const r = cli(['report', 'fleet', 'truck-1', '-46.6333', '-23.5505',
                 '--cat', 'delivering', '--props', '{"plate":"ABC-1234"}']);
  assert.equal(r.status, 0, r.stderr);
  const g = JSON.parse(cli(['get', 'fleet', 'truck-1', '--json']).stdout);
  assert.equal(g.cat, 'delivering');
  assert.equal(g.props.plate, 'ABC-1234');
});

// Exit codes are the whole point of this command; it exists for `if netcluster has ...`
test('has exits 0 when present and 1 when not', () => {
  assert.equal(cli(['has', 'fleet', 'truck-1']).status, 0);
  assert.equal(cli(['has', 'fleet', 'never-seen']).status, 1);
});

test('get on an unknown device fails rather than printing nothing', () => {
  const r = cli(['get', 'fleet', 'never-seen']);
  assert.equal(r.status, 1);
  assert.match(r.stdout, /not registered/);
});

test('import accepts NDJSON on stdin and a JSON array from a file', () => {
  const a = cli(['import', 'fleet', '-'], { input: '{"id":"i1","lng":1,"lat":1}\n{"id":"i2","lng":2,"lat":2}\n' });
  assert.equal(a.status, 0, a.stderr);
  assert.match(a.stdout, /imported 2/);
  const f = join(tmpdir(), `cli-import-${process.pid}.json`);
  writeFileSync(f, JSON.stringify([{ id: 'i3', lng: 3, lat: 3 }]));
  const b = cli(['import', 'fleet', f]);
  assert.equal(b.status, 0, b.stderr);
  assert.match(b.stdout, /imported 1/);
  rmSync(f, { force: true });
  assert.equal(cli(['has', 'fleet', 'i3']).status, 0);
});

test('import rejects a malformed record instead of half-loading', () => {
  const r = cli(['import', 'fleet', '-'], { input: '{"id":"bad","lng":"west","lat":1}\n' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /numeric lng\/lat/);
  assert.equal(cli(['has', 'fleet', 'bad']).status, 1);
});

test('clusters, where, children and leaves chain together', () => {
  const c = JSON.parse(cli(['clusters', 'fleet', '--zoom', '5', '--json']).stdout);
  const total = c.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  assert.ok(total >= 5000, `only ${total} devices accounted for`);

  const w = JSON.parse(cli(['where', 'fleet', 'v42', '--zoom', '5', '--json']).stdout);
  assert.ok(w.properties.cluster, 'v42 should be inside a cluster at zoom 5');

  const cid = String(w.properties.cluster_id);
  const kids = JSON.parse(cli(['children', 'fleet', cid, '--json']).stdout);
  assert.ok(kids.expansion_zoom > 5);
  assert.ok(kids.features.length >= 2);

  const leaves = JSON.parse(cli(['leaves', 'fleet', cid, '--limit', '5', '--json']).stdout);
  assert.equal(leaves.features.length, 5);
});

test('filtering by category reaches the server', () => {
  const all = JSON.parse(cli(['clusters', 'fleet', '--zoom', '5', '--json']).stdout);
  const one = JSON.parse(cli(['clusters', 'fleet', '--zoom', '5', '--cat', 'delivering', '--json']).stdout);
  const sum = (fc) => fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
  assert.ok(sum(one) < sum(all), 'the filter did not reduce anything');
  assert.ok(sum(one) > 0, 'the filter matched nothing');
});

test('tile writes real MVT bytes with --out', () => {
  const f = join(tmpdir(), `cli-tile-${process.pid}.mvt`);
  const r = cli(['tile', 'fleet', '5', '11', '18', '--out', f]);
  assert.equal(r.status, 0, r.stderr);
  const buf = readFileSync(f);
  assert.ok(buf.length > 0);
  assert.equal(buf[0], (3 << 3) | 2, 'not a vector tile');
  rmSync(f, { force: true });
});

test('verify and snapshot', () => {
  const v = cli(['verify', 'fleet']);
  assert.equal(v.status, 0, v.stdout + v.stderr);
  assert.match(v.stdout, /ok/);
  const s = cli(['snapshot', 'fleet']);
  assert.equal(s.status, 0, s.stderr);
  assert.match(s.stdout, /wrote/);
});

// Dropping a populated collection by accident is unrecoverable, so it asks.
test('drop refuses a populated collection without --yes', () => {
  const r = cli(['drop', 'fleet']);
  assert.equal(r.status, 2);
  assert.match(r.stderr, /--yes/);
  assert.equal(cli(['has', 'fleet', 'truck-1']).status, 0, 'it dropped anyway');
});

test('usage errors exit 2, server errors exit 1', () => {
  assert.equal(cli(['report', 'fleet', 'x', 'west', '5']).status, 2);
  assert.equal(cli(['clusters', 'fleet', '--bbox', '1,2']).status, 2);
  assert.equal(cli(['frobnicate']).status, 2);
  assert.equal(cli([]).status, 2, 'bare invocation should print help and exit 2');
  assert.equal(cli(['stats', 'nosuch']).status, 1);
  assert.match(cli(['stats', 'nosuch']).stderr, /no collection/);
});

test('an unreachable server says so rather than hanging', () => {
  const r = cli(['--url', 'http://127.0.0.1:9', 'health', '--timeout', '500']);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /is the server running/);
});

test('help lists every command', () => {
  const r = cli(['help']);
  assert.equal(r.status, 0);
  for (const c of ['health', 'collections', 'create', 'drop', 'stats', 'verify', 'snapshot',
                   'report', 'import', 'seed', 'get', 'has', 'rm',
                   'clusters', 'where', 'children', 'leaves', 'tile', 'watch']) {
    assert.match(r.stdout, new RegExp(`\\b${c}\\b`), `${c} missing from help`);
  }
});

test('every command has per-command help', () => {
  for (const c of ['health', 'create', 'seed', 'clusters', 'tile', 'watch']) {
    const r = cli([c, '--help']);
    assert.equal(r.status, 0, `${c} --help failed`);
    assert.match(r.stdout, new RegExp(`netcluster ${c}`), `${c} --help lacks usage`);
  }
});

test('drop --yes actually drops, snapshot and all', () => {
  const r = cli(['drop', 'fleet', '--yes']);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /dropped/);
  assert.equal(cli(['stats', 'fleet']).status, 1);
});

server.kill();
rmSync(DATA, { recursive: true, force: true });
console.log(`\n${passed} passed`);
