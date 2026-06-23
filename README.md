# @ce-net/sched

Smart job placement for the [CE](https://github.com/ce-net/ce) compute fabric.

`ce-sched` answers one question well: **given a job and the live Fabric Map, which hosts should
run it?** It is the P2/P3 layer of [`ce/docs/compute-fabric.md`](https://github.com/ce-net/ce/blob/main/docs/compute-fabric.md)
built as an *app* on top of CE primitives — it never changes the node. It reads the node's
read-substrate (`/netgraph`, `/atlas`, `/history`, `/beacon`) over plain HTTP and emits a
`PlacementPlan` that the caller dispatches with `/mesh-deploy`.

Zero dependencies. Vanilla JavaScript ES modules. Talks to a local CE node via `fetch()` on
`http://localhost:8844`.

## Quick start

```js
import { planPlacement, CeClient } from "@ce-net/sched";

// One call: resolves the payer (latency origin) from /status, builds the graph, returns advice.
const plan = await planPlacement(
  {
    k: 3,                            // want 3 hosts
    cpuCores: 2, memMb: 4096,        // hard per-host requirements
    requireTags: ["docker", "gpu"], // hard capability gate
    objective: "throughput",        // latency | throughput | balanced | cheap
    redundancy: "verify",           // independent, cross-checked replicas
    demand: { vramMb: { weight: 3, target: 16000 }, fp16Tflops: { weight: 1, target: 40 } },
  },
  { ce: "http://localhost:8844" },   // CeClient | base-URL string | omit for the default
);

// plan.targets is ADVICE. Dispatch is a separate, explicit step you own:
const ce = new CeClient();
for (const t of plan.targets) {
  await ce.meshDeploy({
    node_id: t.nodeId, image: "ghcr.io/me/job:latest", cmd: ["./run"],
    cpu_cores: 2, mem_mb: 4096, duration_secs: 3600,
    bid: "1000000000000000000",     // base-unit STRING — never a float
  });
}
```

Runnable example (offline, no node required):

```
node examples/place-gpu-job.js --mock
```

picks 3 vendor-diverse, low-latency GPU hosts with redundancy verification for a low-trust scenario.
Drop `--mock` (and optionally pass a base URL) to plan against a live node.

`planPlacement` is the convenience facade; `plan(req, ce, opts)` is the fully-injected low-level form
(you pass the payer and client explicitly).

## Why it exists

`swarm` already does trust-tiered placement and K-of redundancy verification (see
[`swarm/README.md`](../swarm/README.md)). `ce-sched` does not duplicate that — it is the *placement
brain* swarm (and any other app) can call. It adds the three things swarm's README lists as "next":

- **Latency-minimizing placement** — rank by measured + Vivaldi-predicted RTT to the payer, and
  co-locate multi-node cohorts via latency regions.
- **Benchmark-aware matching** — consume signed `NodeProfile`s (from `ce-bench`) so a job's
  resource profile (GFLOPS / VRAM / tokens-per-sec) is matched to real hardware, not self-tags.
- **Vendor-aware risk spreading** — diversify across operators / ASNs / regions, cap concentration,
  weight by `/history` reputation, and seed selection from `/beacon` so a host cannot predict or
  steer whether it is chosen.

## How it consumes the read-substrate

| Endpoint | What ce-sched reads | Used for |
|---|---|---|
| `GET /netgraph` | per-peer EWMA RTT + sample counts from this node | latency score; Vivaldi embedding for predicted any-pair RTT; latency regions |
| `GET /atlas` | `cpu_cores`, `mem_mb`, `running_jobs`, `last_seen`, `tags` (+ future signed `profile`) | hard feasibility (headroom/liveness/tags); benchFit (profile, or atlas estimate at reduced confidence); ASN/region grouping |
| `GET /history/:id` | delivered-work counts + earnings (amounts are strings) | trust score; redundancy factor; §9 benchmark cross-check; operator/owner collapse |
| `GET /beacon` | PoW tip `{height, hash}` (verifiable randomness) | deterministic, auditable, un-steerable selection seed (tie-break / softmax) |

The graph is assembled once per `plan()` call anchored at the payer's vantage (a standalone port of
`@ce-net/graph`'s concepts — Vivaldi/MDS embedding, union-find latency regions, Dijkstra — duplicated
in `src/graph.js` so this app stays dependency-free; we never import `ce-ts`).

## Placement scoring + vendor-diversity / risk-spreading

Each feasible candidate gets a static score that is a convex blend of four [0,1] axes, then a
*contextual* diversity penalty is applied during selection:

```
score   = wL·latency + wB·benchFit + wT·trust + wP·price          (the four static axes)
live    = score − wD·diversityPenalty(c, alreadyChosen) + cohortAdjust   (re-ranked each pick)
```

- **latency** — `1 − rtt/rttSoftCapMs`, preferring ground-truth measured RTT over the embedding
  prediction; floors at 0 past the soft cap.
- **benchFit** — demand-weighted, *saturating* match of the host's measured `NodeProfile` to the
  job's `demand` vector (each axis saturates once the host clears the "enough" `target`; surplus
  earns no extra fit credit). No signed profile ⇒ a coarse atlas estimate at `confidence < 1` (the
  trust axis consumes the discount, so unverified hardware is penalized once, not twice).
- **trust** — log-saturating reputation from `/history` delivered work, with a recency factor.
  **§9 cross-check:** a host *claiming* datacenter-grade hardware (high VRAM / TFLOPS / tokens-per-sec)
  with implausibly little delivered work is flagged `benchmarkSuspect` and floored — caught at the app
  layer from public facts alone.
- **price** — cheaper is better, normalized against `priceCapBaseUnits`. All money is **BigInt over
  base-unit strings**; the only float produced is the [0,1] ranking ratio. Never a float amount.

**Vendor diversity / risk spreading** (the `src/vendor.js` layer). Every candidate is tagged with
four correlation keys — `operator` (node id, or on-chain owner when known), `asn` (from a profile
hint or an `asn:<x>` tag), `region` (an O(1) measured-latency region = a same-DC/LAN proxy), and a
union-find `cluster` over all three:

- **Soft penalty** — `diversityPenalty = groupLoad + recentLoad`. `groupLoad` is the fraction of
  already-chosen hosts sharing *any* of this candidate's groups (spreads within one job);
  `recentLoad` is this payer's recent concentration on the candidate's operator (spreads exposure
  *across* jobs over time). Times `wD`, this makes the second-best-but-DIFFERENT host beat the
  second-best-but-SAME host.
- **Hard caps** — `perGroupCap = ceil(effectiveK · maxShare)` (floored at 1). With `k=3` and
  `maxShare = 0.33`, no group may hold more than one replica, so a 3-host plan spans three
  independent operators / ASNs / regions. `redundancy="verify"` additionally enforces **distinct
  operators** unconditionally (verifying across one owner's two boxes proves nothing).
- **Graded relaxation** — when independent hosts are scarce, the selector relaxes the soft caps in a
  fixed `region → asn → operator` ladder (operator never relaxes under `verify`) and records exactly
  which constraints it loosened in `plan.relaxed`, rather than silently violating them.

**Redundancy & confidence.** `redundancy` is `"none"` (trust the single best), `"verify"` (≥2-3
independent, cross-checked replicas — fewer when the best feasible host is high-trust), or a target
confidence in `(0,1)` mapped to the replica count needed given per-host success probability. The
result is `effectiveK`; a pool too small to satisfy it yields a reported `shortfall` (never silent).

**BFT-safe, auditable selection.** The final tie-break (or stochastic softmax) is driven by a PRNG
seeded from `beaconSeed(/beacon, request)` mixed with the payer + a per-request nonce. The seed is
*unpredictable before dispatch* (the beacon is the PoW tip) yet *replayable after* — anyone can
re-run `(beacon, request, candidate set) → same plan`. A host cannot pre-arrange to be selected.

The full algorithm — every formula, the constraint-satisfaction loop, the relaxation ladder, and the
cohort (`spread` / `colocate` / `dag`) modes — is specified in
[`docs/placement-design.md`](docs/placement-design.md).

## Layout

```
ce-sched/
├── src/
│   ├── index.js     barrel + the planPlacement() facade (public entry)
│   ├── types.js     shadowed @ce-net/graph types + PlacementRequest / Candidate / PlacementPlan + defaults
│   ├── ce.js        zero-dep CE HTTP client (status / netgraph / atlas / history / beacon / meshDeploy / meshKill)
│   ├── graph.js     standalone port of @ce-net/graph concepts (RTT/coords -> latency queries)
│   ├── scorer.js    per-candidate scalar score (latency, benchFit, trust, price) + §9 cross-check
│   ├── vendor.js    operator/ASN/region grouping + concentration caps + diversity penalty
│   └── placer.js    plan() — gather → buildGraph → feasible → select → assemble; the brain
├── examples/
│   └── place-gpu-job.js   3 vendor-diverse low-latency GPU hosts, K-redundant, low-trust (run with --mock)
└── docs/
    ├── placement-design.md   the algorithm (read this first)
    ├── node-profile-spec.md  design spec for the node team (the signed NodeProfile this scheduler consumes)
    └── live-mesh-report.md    notes from exercising against the live mesh
```

## The NodeProfile (node-team spec)

ce-sched consumes a signed per-node capability vector it does not produce.
[`docs/node-profile-spec.md`](docs/node-profile-spec.md) is the **design spec handed to the node /
`ce-bench` team** for that struct (the canonical, fuller version lives in
[`ce-bench/docs/nodeprofile-spec.md`](../ce-bench/docs/nodeprofile-spec.md)). Until profiles appear on
the wire, ce-sched degrades gracefully: it scores from `/atlas` capacity + tags at reduced
benchFit/trust confidence. The moment signed profiles arrive (folded into `/atlas` or via the CEP-1
stopgap), benchFit and the §9 cross-check switch to real measured hardware with no code change.

## Status

**App-complete and self-tested.** All five modules (`graph` / `scorer` / `vendor` / `placer`) and the
`planPlacement` facade have offline `__selftest()`s (153 assertions total) plus the index facade test
— all green, no network needed. The example runs offline against a synthetic fleet. What is *not* in
this repo (by design): the signed `NodeProfile` struct + node-side benchmark capsule, which are the
node team's work per the spec above; ce-sched already reads them when present.

```
npm run check                 # node --check src/*.js
node src/index.js             # facade self-test
node examples/place-gpu-job.js --mock
```

## License

MIT © Leif Rydenfalk
