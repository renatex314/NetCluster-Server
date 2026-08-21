# Deploying

## Start with one process

Before anything below: **one process is probably enough.** A single container
holds a fleet of a million devices in a few hundred megabytes and absorbs over two
million position reports per second while serving tiles in a fraction of a
millisecond. Most deployments of this never need a second replica, and a single
replica needs none of the topology rules further down.

| devices | index memory | container request | limit |
|---|---|---|---|
| 100,000 | 28 MB | 128Mi | 256Mi |
| 500,000 | 115 MB | 384Mi | 768Mi |
| 1,000,000 | 180 MB | 512Mi | 1Gi |
| 5,000,000 | ~850 MB | 2Gi | 4Gi |

Ask for roughly triple the steady-state figure. The index grows by doubling its
arrays, so during a rebuild both the old and new allocation are briefly live, and
peak RSS runs well above the settled number — 172 MB peak against a 115 MB
steady state at 500,000 devices.

CPU: give it 2 cores and it will keep up with almost anything. Writes are
serialised through a single lock, so more cores raise read throughput, not ingest.

## There is nothing to persist

No volume, no PVC, no StatefulSet, no backup, no snapshot schedule.

The index holds no truth. The authority for where your devices are is whatever
produces the position reports; this is a materialised view of that stream. A
container that dies is a container you restart, and it refills at about a
microsecond per device — a 500,000 device fleet is back in roughly a second.

That is also why there is no readiness gate on "index warm": an empty index
answers queries correctly, it just answers with fewer markers until the reports
arrive.

## If you run more than one replica

Every replica holds the **complete** index. That inverts the usual load-balancer
assumption:

```
                 ┌──> replica A ──┐
   position ─────┼──> replica B   │   every replica gets EVERY write
   reports       └──> replica C   │
                                  │
   map queries ──────> one of ────┘   any single replica can answer
```

Two rules follow, and both bite late if you miss them.

**1. Writes must not go through a round-robin load balancer.** A balancer sends
each report to one replica, so the others never learn about that device and their
maps are wrong. Address replicas individually and fan out. The Node client does
this for you:

```js
new NetClusterClient({ urls: ['http://pod-a:8080', 'http://pod-b:8080'] })
// writes go to all three; reads go to one
```

In Kubernetes a **headless Service** gives you the per-pod addresses:

```js
import { resolve4 } from 'node:dns/promises';

const HEADLESS = 'netcluster-headless.default.svc.cluster.local';
let client = new NetClusterClient({ urls: [] });

async function refresh() {
  const ips = await resolve4(HEADLESS);
  client = new NetClusterClient({
    urls: ips.map((ip) => `http://${ip}:8080`),
    onReplicaError: (f) => console.warn('replica missed a write', f.map((x) => x.url)),
  });
}
await refresh();
setInterval(refresh, 15_000);   // pods come and go
```

A replica that misses one report self-heals: the device reports again a second
later and the replica catches up. That is why the client resolves as long as *one*
replica accepted, rather than failing the whole ingest because a pod was rolling.

**2. Pin each viewer to one replica.** Replicas that consume the same updates in
slightly different interleavings build slightly different trees, so cluster ids and
groupings differ between them. A viewer whose polls bounce across replicas sees
markers jump between groupings. Fix it at whichever layer you have:

- client side: `client.forViewer(sessionId)`
- Service: `sessionAffinity: ClientIP`
- Ingress/ALB: cookie or consistent-hash affinity

**3. Do not shard geographically.** Splitting the world between replicas does not
work here. An ordinary spatial index can be split by region because an R-tree or
grid query is spatially local, but this hierarchy is globally coupled at coarse
zooms — a cluster at `z=0` spans continents, so a vehicle in Brazil and one in
Angola can share a parent. Split the world and the coarse zooms are simply wrong.

Shard by **collection** instead (fleet A, fleet B, one per tenant), and size a
process so one collection fits inside it.

---

## Docker

```bash
docker build -t netcluster-server .
docker run --rm -p 8080:8080 netcluster-server
```

Roughly a 30 MB image: a distroless base with no shell and no package manager. The
container health check is a flag on the binary itself (`--health`), which is why
the image needs no curl.

```bash
docker compose up          # server + the demo page on :8080
```

| variable | default | |
|---|---|---|
| `NETCLUSTER_ADDR` | `0.0.0.0:8080` | listen address |
| `NETCLUSTER_SWEEP_SECONDS` | `10` | how often to drop expired devices |
| `NETCLUSTER_AUTO_CREATE` | `1` | create a collection on first write |

Turn `NETCLUSTER_AUTO_CREATE` **off** in production. With it on, a typo in a
collection name silently creates an empty collection with default geometry instead
of returning 404, and you debug an empty map instead of reading an error.

---

## Kubernetes

```bash
kubectl apply -f deploy/k8s/
```

`deploy/k8s/` contains a Deployment, two Services (one normal for reads, one
headless for write fan-out), an HPA and a PodDisruptionBudget. The parts that are
specific to this workload:

```yaml
# reads: ordinary ClusterIP, with affinity so a viewer keeps hitting one replica
spec:
  sessionAffinity: ClientIP
  sessionAffinityConfig:
    clientIP:
      timeoutSeconds: 3600
```

```yaml
# writes: headless, so the ingest client can address every pod individually
spec:
  clusterIP: None
  publishNotReadyAddresses: false
```

```yaml
readinessProbe:                 # both probes are plain HTTP; the image has no shell
  httpGet: { path: /healthz, port: 8080 }
  periodSeconds: 5
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
  periodSeconds: 10
  failureThreshold: 3
```

Scale on **CPU or request rate, never memory.** Memory tracks fleet size, which
does not change when you add replicas — every replica holds the same full index —
so a memory-based HPA will scale up and never scale back down.

`terminationGracePeriodSeconds: 15` is plenty. There is nothing to flush; the
process closes its listener, finishes in-flight requests, and exits.

### Feeding it

The server ingests over HTTP, so whatever consumes your stream does the fan-out.
A small Deployment running the Node client works:

```
Kafka / PubSub / MQTT ──> ingest Deployment (netcluster-client) ──> all replicas
```

Run **one** ingest replica per partition of your stream, not one per netcluster
pod. Each ingest worker fans its partition out to every netcluster replica.

---

## Google Cloud

### GKE — the straightforward option

```bash
gcloud artifacts repositories create netcluster --repository-format=docker --location=us-central1
docker build -t us-central1-docker.pkg.dev/$PROJECT/netcluster/server:v1 .
docker push us-central1-docker.pkg.dev/$PROJECT/netcluster/server:v1

gcloud container clusters get-credentials my-cluster --region us-central1
kubectl apply -f deploy/k8s/
```

Apply the Kubernetes section above as written. For the stream, Pub/Sub into an
ingest Deployment that fans out over the headless Service.

### Cloud Run — read the caveat first

**Cloud Run is a poor fit for a multi-instance deployment of this, and the reason
is not obvious.** Cloud Run gives you no way to address an individual instance.
Every request goes through its load balancer, so:

- a position report reaches exactly one instance; the others never learn about
  that device and serve a map missing it
- a scaled-up instance starts with an empty index and serves near-empty tiles
  until the fleet reports again
- scale-to-zero throws the whole index away

It works in exactly one configuration:

```bash
gcloud run deploy netcluster \
  --image us-central1-docker.pkg.dev/$PROJECT/netcluster/server:v1 \
  --min-instances 1 --max-instances 1 \
  --cpu 2 --memory 1Gi --concurrency 200 \
  --set-env-vars NETCLUSTER_AUTO_CREATE=0 \
  --no-cpu-throttling
```

`--min-instances 1 --max-instances 1` is load-bearing, not a cost setting. So is
`--no-cpu-throttling`: without it Cloud Run throttles the CPU between requests and
the expiry sweep stops running, so stale devices accumulate.

One instance handles a large fleet, so this is a real option — you are trading
availability, not capacity. If you need more than one instance, use GKE.

The same reasoning rules out anything else that hides individual instances behind
a balancer: App Engine, AWS App Runner, Lambda.

---

## AWS

### ECS on Fargate

```bash
aws ecr create-repository --repository-name netcluster-server
docker build -t $ACCOUNT.dkr.ecr.$REGION.amazonaws.com/netcluster-server:v1 .
docker push $ACCOUNT.dkr.ecr.$REGION.amazonaws.com/netcluster-server:v1
```

Task sizing for a million devices: **2 vCPU, 2 GB**. Nothing to mount.

Two settings matter:

- **Enable ECS Service Connect or Cloud Map service discovery.** That gives each
  task its own DNS record, which is how the ingest worker addresses tasks
  individually. Without it you have the Cloud Run problem: the ALB sends each
  report to one task and the rest drift.
- **Turn on target-group stickiness** on the ALB for read traffic
  (`stickiness.enabled=true`, `stickiness.type=lb_cookie`), so a viewer keeps
  hitting one task.

Health check on the target group: HTTP `GET /healthz`, 5 s interval, 2 healthy /
2 unhealthy. The container-level `HEALTHCHECK` in the image also works and needs no
extra configuration.

Feed it from MSK or Kinesis into a separate ingest service running the Node
client, resolving tasks through Cloud Map and fanning out.

### EKS

Identical to the Kubernetes section. Use the AWS Load Balancer Controller and put
stickiness on the read Service:

```yaml
annotations:
  service.beta.kubernetes.io/aws-load-balancer-type: nlb
  service.beta.kubernetes.io/aws-load-balancer-target-group-attributes: stickiness.enabled=true
```

---

## Operating it

### Set a TTL

A vehicle that stops reporting does not stop existing in the index. Clusters
quietly fill with ghosts and every count on the map reads high, which is the kind
of wrong nobody notices for a week.

```js
await client.createCollection('fleet', { ttlSeconds: 300 });
```

Set it to a few times your reporting interval. The sweep runs off the request path
and drops devices in small batches, so it never stalls queries.

### Batch size is the read-latency knob

One request holds the write lock for its whole duration, so every reader waits
behind it. Measured at 200,000 devices, four concurrent readers:

| batch | write throughput | reader p99 |
|---|---|---|
| 100 | 627k reports/s | 0.43 ms |
| 500 | 1,401k | 0.40 ms |
| **1000** | **1,693k** | **0.52 ms** |
| 2000 | 1,936k | 0.65 ms |
| 5000 | 2,063k | 1.28 ms |
| 20000 | 2,176k | 2.46 ms |

1000 is the client default: 78% of peak throughput for half a millisecond of
stall. Going to 20,000 buys 5% more throughput and costs five times the stall.

### Cache tiles

Tile URLs are stable, so an HTTP cache in front actually hits. The server already
sends `Cache-Control: public, max-age=2`. At coarse zooms one query then serves
every viewer looking at that region — put CloudFront, Cloud CDN or nginx in front
of the read path and the tile load collapses.

Keep the window short. The data moves; two seconds of staleness is invisible on a
map and two minutes is not.

### Watch

`/metrics` is Prometheus text. The two worth alerting on:

- `netcluster_fast_move_ratio` — the fraction of position reports absorbed without
  restructuring the tree. Healthy is above 0.9. A sustained drop means your devices
  are jumping further per report than the index expects, and updates are costing
  several microseconds instead of one.
- `netcluster_devices` — flat or falling when it should be rising usually means an
  ingest worker died, or a TTL shorter than your reporting interval.

`GET /v1/collections/{name}/verify` re-derives every structural invariant from
scratch. It is `O(N²)` and will take minutes on a large index — a staging tool, not
something to put on a dashboard.
