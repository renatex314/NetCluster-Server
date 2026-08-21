#!/usr/bin/env node
/**
 * netcluster -- command line for netcluster-server.
 *
 * A thin layer over the client in this package, deliberately: the client already
 * handles replica fan-out, retries, and the error codes that distinguish "no such
 * device" from "no such collection". Re-implementing any of that here would mean
 * two behaviours to keep in step.
 *
 * Zero dependencies, like the rest of this package. The argument parser below is
 * a hundred lines and does what a CLI of this size needs.
 */

import { NetClusterClient, NetClusterError } from './index.js';
import { readFileSync } from 'node:fs';

const VERSION = '0.1.0';

// ---------------------------------------------------------------- plumbing --

const COLOR = process.stdout.isTTY && !process.env.NO_COLOR;
const c = (code, s) => (COLOR ? `\x1b[${code}m${s}\x1b[0m` : String(s));
const bold = (s) => c('1', s);
const dim = (s) => c('2', s);
const red = (s) => c('31', s);
const green = (s) => c('32', s);
const yellow = (s) => c('33', s);

/** Exit codes worth distinguishing: 2 means you typed it wrong, 1 means it failed. */
const EXIT_OK = 0;
const EXIT_FAIL = 1;
const EXIT_USAGE = 2;

class UsageError extends Error {}

/**
 * `--flag value`, `--flag=value`, `--bool`, and positionals. Everything after a
 * bare `--` is positional, so an id that starts with a dash is still reachable.
 */
function parseArgs(argv) {
  const flags = {};
  const positional = [];
  let onlyPositional = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (onlyPositional) {
      positional.push(a);
    } else if (a === '--') {
      onlyPositional = true;
    } else if (a.startsWith('--')) {
      const eq = a.indexOf('=');
      if (eq !== -1) {
        flags[a.slice(2, eq)] = a.slice(eq + 1);
      } else {
        const name = a.slice(2);
        const next = argv[i + 1];
        if (next === undefined || next.startsWith('--')) flags[name] = true;
        else flags[name] = argv[++i];
      }
    } else if (a === '-h') {
      flags.help = true;
    } else {
      positional.push(a);
    }
  }
  return { flags, positional };
}

function num(flags, name, fallback) {
  if (flags[name] === undefined) return fallback;
  const v = Number(flags[name]);
  if (!Number.isFinite(v)) throw new UsageError(`--${name} must be a number, got ${flags[name]}`);
  return v;
}

function out(obj, flags) {
  if (flags.json) {
    console.log(JSON.stringify(obj, null, flags.compact ? 0 : 2));
    return true;
  }
  return false;
}

/** Key/value block, keys right-aligned so the values line up. */
function kv(pairs) {
  const w = Math.max(...pairs.map(([k]) => k.length));
  for (const [k, v] of pairs) console.log(`  ${dim(k.padStart(w))}  ${v}`);
}

/** Left-aligned columns sized to their content. */
function table(rows, headers) {
  if (!rows.length) return;
  const all = headers ? [headers, ...rows] : rows;
  const w = all[0].map((_, i) => Math.max(...all.map((r) => String(r[i] ?? '').length)));
  const line = (r, f = (x) => x) =>
    console.log('  ' + r.map((cell, i) => f(String(cell ?? '').padEnd(w[i]))).join('  ').trimEnd());
  if (headers) line(headers, dim);
  for (const r of rows) line(r);
}

const n = (x) => Number(x ?? 0).toLocaleString();
const mb = (b) => (Number(b ?? 0) / 1e6).toFixed(1) + ' MB';

function ago(ms) {
  if (!ms) return 'never';
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  return `${Math.round(s / 3600)}h ago`;
}

function parseBbox(v) {
  if (!v) return undefined;
  const p = String(v).split(',').map(Number);
  if (p.length !== 4 || p.some((x) => !Number.isFinite(x))) {
    throw new UsageError('--bbox needs four numbers: west,south,east,north');
  }
  return p;
}

function parseProps(v) {
  if (v === undefined) return undefined;
  let raw = String(v);
  if (raw === '-') raw = readFileSync(0, 'utf8');
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new UsageError(`--props is not valid JSON: ${e.message}`);
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new UsageError('--props must be a JSON object');
  }
  return parsed;
}

/** A JSON array, or newline-delimited JSON, from a file or stdin. */
function readRecords(path) {
  const raw = path === '-' || path === undefined ? readFileSync(0, 'utf8') : readFileSync(path, 'utf8');
  const t = raw.trim();
  if (!t) return [];
  if (t.startsWith('[')) return JSON.parse(t);
  return t
    .split('\n')
    .filter((l) => l.trim())
    .map((l, i) => {
      try {
        return JSON.parse(l);
      } catch (e) {
        throw new UsageError(`line ${i + 1} is not valid JSON: ${e.message}`);
      }
    });
}

// ---------------------------------------------------------------- commands --

const commands = {};
const cmd = (name, spec) => (commands[name] = spec);

cmd('health', {
  usage: 'health',
  blurb: 'is the server up, and what is it holding',
  async run(nc, _pos, flags) {
    const h = await nc.health();
    if (out(h, flags)) return;
    kv([
      ['status', h.status === 'ok' ? green(h.status) : red(h.status)],
      ['collections', n(h.collections)],
      ['devices', n(h.devices)],
      ['uptime', `${Math.round(h.uptime_ms / 1000)}s`],
      ['persistence', h.persistence ? green('on') : dim('off')],
    ]);
  },
});

cmd('collections', {
  usage: 'collections',
  blurb: 'list every collection with its size and geometry',
  async run(nc, _pos, flags) {
    const { collections } = await nc.listCollections();
    if (out(collections, flags)) return;
    if (!collections.length) return console.log(dim('  no collections'));
    table(
      collections.map((s) => [
        s.name,
        n(s.devices),
        mb(s.memory_bytes),
        `z${s.max_zoom}`,
        `r${s.radius}`,
        s.ttl_seconds ? `${s.ttl_seconds}s` : dim('none'),
        s.categories.length ? s.categories.join(',') : dim('-'),
      ]),
      ['NAME', 'DEVICES', 'MEMORY', 'MAXZOOM', 'RADIUS', 'TTL', 'CATEGORIES']
    );
  },
});

cmd('create', {
  usage: 'create <name> [--radius 40] [--extent 512] [--max-zoom 16] [--hysteresis 0.25]\n' +
         '                      [--ttl 300] [--categories idle,enroute] [--max-props-bytes 1024]',
  blurb: 'create a collection (idempotent; 409 if the geometry differs)',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('create needs a collection name');
    const cfg = {};
    if (flags.radius !== undefined) cfg.radius = num(flags, 'radius');
    if (flags.extent !== undefined) cfg.extent = num(flags, 'extent');
    if (flags['max-zoom'] !== undefined) cfg.maxZoom = num(flags, 'max-zoom');
    if (flags.hysteresis !== undefined) cfg.hysteresis = num(flags, 'hysteresis');
    if (flags.ttl !== undefined) cfg.ttlSeconds = num(flags, 'ttl');
    if (flags['max-props-bytes'] !== undefined) cfg.maxPropsBytes = num(flags, 'max-props-bytes');
    if (flags.categories !== undefined) {
      cfg.categories = String(flags.categories).split(',').map((s) => s.trim()).filter(Boolean);
    }
    const r = await nc.createCollection(name, cfg);
    if (out(r, flags)) return;
    console.log(r.created ? `  ${green('created')} ${name}` : `  ${dim('exists')}  ${name} (same geometry)`);
  },
});

cmd('drop', {
  usage: 'drop <name> [--yes]',
  blurb: 'delete a collection and its snapshot',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('drop needs a collection name');
    if (!flags.yes) {
      const s = await nc.stats(name).catch(() => null);
      if (s && s.devices > 0) {
        throw new UsageError(
          `${name} holds ${n(s.devices)} devices. Re-run with --yes to drop it.`
        );
      }
    }
    const r = await nc.dropCollection(name);
    if (out(r, flags)) return;
    console.log(`  ${green('dropped')} ${name} (${n(r.devices)} devices${r.snapshot_removed ? ', snapshot removed' : ''})`);
  },
});

cmd('stats', {
  usage: 'stats <name>',
  blurb: 'everything the server knows about one collection',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('stats needs a collection name');
    const s = await nc.stats(name);
    if (out(s, flags)) return;
    kv([
      ['devices', n(s.devices)],
      ['memory', mb(s.memory_bytes)],
      ['grid entries', n(s.grid_entries)],
      ['geometry', `radius ${s.radius}, maxZoom ${s.max_zoom}`],
      ['categories', s.categories.length ? s.categories.join(', ') : dim('none')],
      ['ttl', s.ttl_seconds ? `${s.ttl_seconds}s` : dim('disabled')],
      ['reports', n(s.ingested)],
      ['queries', n(s.queries)],
      ['expired', n(s.expired)],
      ['fast-path moves', `${s.moves_fast_pct.toFixed(1)}%`],
      ['props', `${mb(s.props_bytes)} of ${n(s.max_props_bytes)} bytes/device allowed`],
      ['restored at boot', n(s.restored)],
      ['last snapshot', s.last_snapshot_ms ? `${ago(s.last_snapshot_ms)} (${mb(s.last_snapshot_bytes)})` : dim('never')],
      ['snapshot failures', s.snapshot_failures ? red(n(s.snapshot_failures)) : '0'],
      ['uptime', `${Math.round(s.uptime_ms / 1000)}s`],
    ]);
    if (s.moves_fast_pct > 0 && s.moves_fast_pct < 80) {
      console.log(`\n  ${yellow('note')} only ${s.moves_fast_pct.toFixed(0)}% of moves take the fast path.`);
      console.log(`       devices are jumping further per report than the geometry expects.`);
    }
  },
});

cmd('verify', {
  usage: 'verify <name>',
  blurb: 're-derive every structural invariant (admin only, O(N squared))',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('verify needs a collection name');
    const v = await nc.verify(name);
    if (out(v, flags)) return;
    if (v.ok) console.log(`  ${green('ok')} ${v.detail ?? ''}`);
    else {
      console.log(`  ${red('BROKEN')} ${v.violation}`);
      process.exitCode = EXIT_FAIL;
    }
  },
});

cmd('snapshot', {
  usage: 'snapshot <name>',
  blurb: 'write a snapshot now (needs NETCLUSTER_DATA_DIR on the server)',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('snapshot needs a collection name');
    const r = await nc.snapshot(name);
    if (out(r, flags)) return;
    console.log(`  ${green('wrote')} ${n(r.bytes)} bytes for ${name}`);
  },
});

cmd('report', {
  usage: 'report <name> <id> <lng> <lat> [--cat X] [--props \'{"k":"v"}\']',
  blurb: 'report one position',
  async run(nc, [name, id, lng, lat], flags) {
    if (!name || !id || lng === undefined || lat === undefined) {
      throw new UsageError('report needs <name> <id> <lng> <lat>');
    }
    const p = { id, lng: Number(lng), lat: Number(lat) };
    if (!Number.isFinite(p.lng) || !Number.isFinite(p.lat)) {
      throw new UsageError('lng and lat must be numbers');
    }
    if (flags.cat !== undefined) p.cat = flags.cat;
    const props = parseProps(flags.props);
    if (props !== undefined) p.props = props;
    const r = await nc.report(name, [p]);
    if (out(r, flags)) return;
    console.log(`  ${green('ok')} ${id} -> ${n(r.devices)} devices in ${name}`);
  },
});

cmd('import', {
  usage: 'import <name> [file] [--batch 1000]',
  blurb: 'bulk load a JSON array or NDJSON of reports (reads stdin with - or no file)',
  async run(nc, [name, file], flags) {
    if (!name) throw new UsageError('import needs a collection name');
    const records = readRecords(file);
    if (!records.length) return console.log(dim('  nothing to import'));
    for (const [i, r] of records.entries()) {
      if (!r || typeof r.id !== 'string' || !Number.isFinite(r.lng) || !Number.isFinite(r.lat)) {
        throw new UsageError(`record ${i} needs a string id and numeric lng/lat`);
      }
    }
    const batch = num(flags, 'batch', 1000);
    const t0 = Date.now();
    const r = await nc.report(name, records, { maxBatch: batch });
    const ms = Date.now() - t0;
    if (out({ ...r, ms }, flags)) return;
    const rate = Math.round((records.length / Math.max(ms, 1)) * 1000);
    console.log(`  ${green('imported')} ${n(r.accepted)} reports in ${ms} ms (${n(rate)}/s), ${n(r.devices)} devices`);
  },
});

cmd('seed', {
  usage: 'seed <name> [--count 10000] [--around -46.63,-23.55] [--spread 0.9] [--categories]',
  blurb: 'generate a simulated fleet, for demos and load tests',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('seed needs a collection name');
    const count = num(flags, 'count', 10000);
    const spread = num(flags, 'spread', 0.9);
    const HUBS = flags.around
      ? [String(flags.around).split(',').map(Number)]
      : [[-46.63, -23.55], [-43.17, -22.9], [-47.88, -15.79], [-38.52, -3.73], [-34.88, -8.05]];
    const cats = flags.categories ? String(flags.categories).split(',') : null;
    const points = Array.from({ length: count }, (_, i) => {
      const h = HUBS[i % HUBS.length];
      const p = {
        id: `${flags.prefix ?? 'v'}${i}`,
        lng: +(h[0] + (Math.random() - 0.5) * spread).toFixed(6),
        lat: +(h[1] + (Math.random() - 0.5) * spread).toFixed(6),
      };
      if (cats) p.cat = cats[i % cats.length];
      return p;
    });
    const t0 = Date.now();
    const r = await nc.report(name, points, { maxBatch: num(flags, 'batch', 1000) });
    const ms = Date.now() - t0;
    if (out({ ...r, ms }, flags)) return;
    console.log(`  ${green('seeded')} ${n(r.accepted)} devices in ${ms} ms (${n(Math.round((count / Math.max(ms, 1)) * 1000))}/s)`);
  },
});

cmd('rm', {
  usage: 'rm <name> <id>',
  blurb: 'remove one device',
  async run(nc, [name, id], flags) {
    if (!name || !id) throw new UsageError('rm needs <name> <id>');
    const r = await nc.remove(name, id);
    if (out(r, flags)) return;
    console.log(r.removed ? `  ${green('removed')} ${id}` : `  ${dim('not registered')} ${id}`);
    if (!r.removed) process.exitCode = EXIT_FAIL;
  },
});

cmd('get', {
  usage: 'get <name> <id>',
  blurb: 'position, category, staleness and properties for one device',
  async run(nc, [name, id], flags) {
    if (!name || !id) throw new UsageError('get needs <name> <id>');
    const d = await nc.getDevice(name, id);
    if (d === null) {
      if (out(null, flags)) return;
      console.log(`  ${dim('not registered')} ${id}`);
      process.exitCode = EXIT_FAIL;
      return;
    }
    if (out(d, flags)) return;
    kv([
      ['id', d.id],
      ['position', `${d.lng.toFixed(6)}, ${d.lat.toFixed(6)}`],
      ['category', d.cat ?? dim(`(index ${d.cat_index})`)],
      ['last report', `${Math.round(d.age_ms / 1000)}s ago`],
      ['props', d.props ? JSON.stringify(d.props) : dim('none')],
    ]);
  },
});

cmd('has', {
  usage: 'has <name> <id>',
  blurb: 'is this device registered (exit 0 yes, 1 no) -- for scripts',
  async run(nc, [name, id], flags) {
    if (!name || !id) throw new UsageError('has needs <name> <id>');
    const yes = await nc.has(name, id);
    if (out({ has: yes }, flags)) return void (process.exitCode = yes ? EXIT_OK : EXIT_FAIL);
    console.log(yes ? green('true') : 'false');
    process.exitCode = yes ? EXIT_OK : EXIT_FAIL;
  },
});

cmd('clusters', {
  usage: 'clusters <name> [--zoom 8] [--bbox w,s,e,n] [--cat X] [--limit 20]',
  blurb: 'what would be drawn on the map at this zoom',
  async run(nc, [name], flags) {
    if (!name) throw new UsageError('clusters needs a collection name');
    const zoom = num(flags, 'zoom', 8);
    const fc = await nc.getClusters(name, { zoom, bbox: parseBbox(flags.bbox), cat: flags.cat });
    if (out(fc, flags)) return;
    const total = fc.features.reduce((a, f) => a + (f.properties.point_count ?? 1), 0);
    const limit = num(flags, 'limit', 20);
    const rows = fc.features.slice(0, limit).map((f) => [
      f.properties.cluster ? `cluster ${f.properties.cluster_id}` : f.id,
      f.properties.cluster ? n(f.properties.point_count) : '1',
      f.geometry.coordinates[0].toFixed(4),
      f.geometry.coordinates[1].toFixed(4),
    ]);
    table(rows, ['WHAT', 'COUNT', 'LNG', 'LAT']);
    if (fc.features.length > limit) console.log(dim(`  ... ${n(fc.features.length - limit)} more`));
    console.log(`\n  ${bold(n(fc.features.length))} markers covering ${bold(n(total))} devices at zoom ${zoom}`);
  },
});

cmd('where', {
  usage: 'where <name> <id> [--zoom 8]',
  blurb: 'which marker is this device drawn inside',
  async run(nc, [name, id], flags) {
    if (!name || !id) throw new UsageError('where needs <name> <id>');
    const f = await nc.deviceCluster(name, id, num(flags, 'zoom', 8));
    if (out(f, flags)) return;
    const p = f.properties;
    if (p.cluster) {
      console.log(`  inside cluster ${bold(p.cluster_id)} of ${bold(n(p.point_count))} devices`);
      console.log(dim(`  at ${f.geometry.coordinates.map((x) => x.toFixed(4)).join(', ')}`));
      console.log(dim(`  netcluster children ${name} ${p.cluster_id}  # to expand it`));
    } else {
      console.log(`  drawn on its own at ${f.geometry.coordinates.map((x) => x.toFixed(4)).join(', ')}`);
    }
  },
});

cmd('children', {
  usage: 'children <name> <cluster-id>',
  blurb: 'one expansion step below a cluster',
  async run(nc, [name, cid], flags) {
    if (!name || cid === undefined) throw new UsageError('children needs <name> <cluster-id>');
    const r = await nc.getChildren(name, cid);
    if (out(r, flags)) return;
    table(
      r.features.map((f) => [
        f.properties.cluster ? `cluster ${f.properties.cluster_id}` : f.id,
        f.properties.cluster ? n(f.properties.point_count) : '1',
        f.geometry.coordinates[0].toFixed(4),
        f.geometry.coordinates[1].toFixed(4),
      ]),
      ['WHAT', 'COUNT', 'LNG', 'LAT']
    );
    console.log(`\n  splits into ${bold(r.features.length)} at zoom ${bold(r.expansion_zoom)}`);
  },
});

cmd('leaves', {
  usage: 'leaves <name> <cluster-id> [--limit 20] [--offset 0]',
  blurb: 'the individual devices inside a cluster',
  async run(nc, [name, cid], flags) {
    if (!name || cid === undefined) throw new UsageError('leaves needs <name> <cluster-id>');
    const r = await nc.getLeaves(name, cid, {
      limit: num(flags, 'limit', 20),
      offset: num(flags, 'offset', 0),
    });
    if (out(r, flags)) return;
    table(
      r.features.map((f) => [f.id, f.geometry.coordinates[0].toFixed(4), f.geometry.coordinates[1].toFixed(4)]),
      ['DEVICE', 'LNG', 'LAT']
    );
  },
});

cmd('tile', {
  usage: 'tile <name> <z> <x> <y> [--cat X] [--out file.mvt]',
  blurb: 'fetch one vector tile; --out writes the raw MVT bytes',
  async run(nc, [name, z, x, y], flags) {
    if (!name || z === undefined || x === undefined || y === undefined) {
      throw new UsageError('tile needs <name> <z> <x> <y>');
    }
    const args = [name, Number(z), Number(x), Number(y)];
    if (flags.out) {
      const buf = await nc.getTile(...args, { cat: flags.cat });
      const { writeFileSync } = await import('node:fs');
      writeFileSync(String(flags.out), buf);
      console.log(`  ${green('wrote')} ${n(buf.length)} bytes to ${flags.out}`);
      return;
    }
    const j = await nc.getTile(...args, { cat: flags.cat, format: 'json' });
    if (out(j, flags)) return;
    table(
      j.features.map((f) => [
        f.properties.cluster ? `cluster ${f.properties.cluster_id}` : f.id,
        f.properties.cluster ? n(f.properties.point_count) : '1',
        Math.round(f.geometry.coordinates[0]),
        Math.round(f.geometry.coordinates[1]),
      ]),
      ['WHAT', 'COUNT', 'X', 'Y']
    );
    console.log(`\n  ${bold(j.features.length)} markers, coordinates in tile-extent units (extent ${j.extent})`);
  },
});

cmd('watch', {
  usage: 'watch [name] [--interval 2]',
  blurb: 'live view of ingest rate, memory and snapshot health',
  async run(nc, [name], flags) {
    const every = num(flags, 'interval', 2) * 1000;
    let last = null;
    let lastAt = 0;
    const tick = async () => {
      let rows;
      try {
        const list = name ? [await nc.stats(name)] : (await nc.listCollections()).collections;
        const now = Date.now();
        rows = list.map((s) => {
          const prev = last?.[s.name];
          const dt = (now - lastAt) / 1000;
          const rate = prev && dt > 0 ? Math.round((s.ingested - prev) / dt) : 0;
          return [
            s.name,
            n(s.devices),
            prev ? `${n(rate)}/s` : dim('...'),
            mb(s.memory_bytes),
            `${s.moves_fast_pct.toFixed(0)}%`,
            s.snapshot_failures ? red(n(s.snapshot_failures)) : ago(s.last_snapshot_ms),
          ];
        });
        last = Object.fromEntries(list.map((s) => [s.name, s.ingested]));
        lastAt = now;
      } catch (e) {
        rows = null;
        process.stdout.write('\x1b[2J\x1b[H');
        console.log(red(`  ${e.message}`));
        return;
      }
      process.stdout.write('\x1b[2J\x1b[H');
      console.log(bold(`  netcluster  ${new Date().toLocaleTimeString()}`) + dim('   ctrl-c to stop'));
      console.log();
      table(rows, ['NAME', 'DEVICES', 'REPORTS', 'MEMORY', 'FAST', 'SNAPSHOT']);
    };
    await tick();
    const t = setInterval(tick, every);
    process.on('SIGINT', () => {
      clearInterval(t);
      process.stdout.write('\n');
      process.exit(EXIT_OK);
    });
    await new Promise(() => {});
  },
});

// -------------------------------------------------------------------- main --

function help() {
  console.log(`${bold('netcluster')} ${dim(VERSION)} -- manage a netcluster-server

${dim('USAGE')}
  netcluster <command> [args] [--url http://host:8080] [--json]

${dim('SERVER')}`);
  for (const k of ['health', 'collections', 'watch']) console.log(`  ${k.padEnd(12)} ${dim(commands[k].blurb)}`);
  console.log(`\n${dim('COLLECTIONS')}`);
  for (const k of ['create', 'drop', 'stats', 'verify', 'snapshot']) console.log(`  ${k.padEnd(12)} ${dim(commands[k].blurb)}`);
  console.log(`\n${dim('DEVICES')}`);
  for (const k of ['report', 'import', 'seed', 'get', 'has', 'rm']) console.log(`  ${k.padEnd(12)} ${dim(commands[k].blurb)}`);
  console.log(`\n${dim('QUERIES')}`);
  for (const k of ['clusters', 'where', 'children', 'leaves', 'tile']) console.log(`  ${k.padEnd(12)} ${dim(commands[k].blurb)}`);
  console.log(`
${dim('OPTIONS')}
  --url <u>      server address, or NETCLUSTER_URL (default http://localhost:8080)
  --json         machine-readable output
  --timeout <ms> per-request timeout (default 10000)
  -h, --help     help for a command: netcluster <command> --help

${dim('EXAMPLES')}
  netcluster create fleet --categories idle,enroute,delivering --ttl 300
  netcluster seed fleet --count 50000
  netcluster clusters fleet --zoom 6
  netcluster where fleet v42 --zoom 10
  netcluster watch
  netcluster stats fleet --json | jq .devices`);
}

async function main() {
  const { flags, positional } = parseArgs(process.argv.slice(2));
  const name = positional.shift();

  if (flags.version) return console.log(VERSION);
  if (!name || name === 'help' || (flags.help && !name)) {
    help();
    return void (process.exitCode = name ? EXIT_OK : EXIT_USAGE);
  }
  const spec = commands[name];
  if (!spec) {
    console.error(red(`unknown command: ${name}`));
    const near = Object.keys(commands).filter((k) => k.startsWith(name[0]));
    if (near.length) console.error(dim(`did you mean: ${near.join(', ')}?`));
    console.error(dim('netcluster help'));
    return void (process.exitCode = EXIT_USAGE);
  }
  if (flags.help) {
    console.log(`${bold(name)} -- ${spec.blurb}\n\n  netcluster ${spec.usage}`);
    return;
  }

  const nc = new NetClusterClient({
    url: flags.url ?? process.env.NETCLUSTER_URL ?? 'http://localhost:8080',
    timeoutMs: num(flags, 'timeout', 10000),
  });
  await spec.run(nc, positional, flags);
}

main().catch((e) => {
  if (e instanceof UsageError) {
    console.error(red(e.message));
    process.exitCode = EXIT_USAGE;
  } else if (e instanceof NetClusterError) {
    console.error(red(e.body?.error ?? e.message));
    // The distinction the server goes out of its way to make is worth surfacing.
    if (e.body?.code === 'no_such_collection') console.error(dim('  netcluster collections'));
    if (e.body?.code === 'persistence_disabled') {
      console.error(dim('  the server needs NETCLUSTER_DATA_DIR set to keep snapshots'));
    }
    if (e.status === 0) console.error(dim('  is the server running? --url or NETCLUSTER_URL'));
    process.exitCode = EXIT_FAIL;
  } else {
    console.error(red(e.message));
    process.exitCode = EXIT_FAIL;
  }
});
