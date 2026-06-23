# Live-mesh test report — ce-sched + ce-bench against a running CE node

Date: 2026-06-23
Node under test: local `ce start --light`, HTTP API `http://localhost:8844`
Apps tested: `@ce-net/sched` (placement) and `@ce-net/bench` (benchmark/scoreboard), both
vanilla-JS ESM, zero-dependency. No `ce`/`ce-ts`/`ce-fabric` files were touched.

This documents exactly what the live mesh returned, what ran, what came back empty (and why),
and what additional nodes/data are needed for a full end-to-end test.

---

## 1. What the live node returned (raw probe)

All endpoints reached on `http://localhost:8844` (the api.md default; the node was listening there).

| Endpoint | Result |
|---|---|
| `GET /status` | `{ node_id: c0be11e0…be456, height: ~134818, difficulty: 0, balance: "134818000000000000000000000" }` — alive, mining (balance grows ~1 credit/block) |
| `GET /beacon` | `{ height: ~134818, hash: "3ec6afd7…b555" }` — **works**; live verifiable randomness, changes each block |
| `GET /atlas` | `[]` — **empty**. No peer capacity advertised (single-node mesh; self is not in its own atlas in light mode) |
| `GET /netgraph` | **HTTP 404** — endpoint not present on this build. Compute-fabric P0 (libp2p ping + `/netgraph`) is either not in this binary or disabled in `--light` |
| `GET /netgraph/rtt?to=…` | empty body |
| `GET /history/<self id>` | `{ jobs_hosted:0, jobs_paid:0, heartbeats_hosted:0, heartbeats_paid:0, expiries:0, earned:"0", spent:"0", first_height:0, last_height:0 }` — **works**, all zeros (node has hosted/paid nothing) |
| ce-hub `http://localhost:8970/stats`, `/nodes` | connection refused — **ce-hub is not running**; no browser nodes |

Money note: `/status.balance` and `/history.earned/spent` came back as decimal base-unit
**strings** as designed; nothing was parsed to a float anywhere in the test.

---

## 2. What ran, and the results

### 2.1 Offline self-tests (both apps) — PASS

Both apps ship deterministic offline self-tests; both pass on Node v22:

- `ce-sched/src/placer.js __selftest()` → `{ ok: true, checks: 34 }`
  (feasibility filter, vendor spread, low-RTT-first, verify→distinct operators, short-plan
  shortfall, beacon-seed determinism, diversity-penalty lever).
- `ce-bench/src/fabricstats.js __selftest()` + `__selftestAsync()` → pass
  (median, robustSum clamp, aggregateCompute, perfScore, dedupeLatest, meshHealth star+disjoint,
  fuseEdges sample-weighted mean, profileFromSignal round-trip, historyTrust no-float-money,
  full collect→dedupe→aggregate→mesh→perf path + node-served preference).

`node --check` is clean on every `src/*.js` in both apps.

### 2.2 Full pipeline against the LIVE node — runs, degrades gracefully

Driving the **real** `CeClient` + `plan()` + `computeFabricStats()` against the live node
(with a tolerant fetch that maps the `/netgraph` 404 → `[]`, since these pre-P0/proposed
endpoints are absent), every stage executed without error on the data the node does serve:

- `plan({ payer: <self>, k:3, requireTags:["docker"], redundancy:"verify" }, ce)` →
  `{ targets: [], effectiveK: 3, shortfall: 3, beacon: <live beacon>, rejected: [] }`.
  Correct: an empty atlas yields zero candidates, so the placer reports a 3-replica
  **shortfall** rather than inventing hosts. `beaconSeed` = a stable 32-bit int from the live
  beacon.
- `feasible(liveAtlas, req, graph, now)` → `0` candidates (atlas empty), with and without
  `allowSelf`.
- `computeFabricStats(ce)` → all-zero `FabricStats` with `mesh.reachable_frac = 1` (vacuously
  full for ≤1 node), `regions: 0`, `nodes: 0`. The verified variant is likewise all-zero.

This proves the **plumbing is correct end-to-end on real data**: status, beacon, atlas, and
history are read through the apps' own clients; the algorithms produce the right *empty-mesh*
answers (shortfall, zero stats) instead of throwing.

### 2.3 Algorithms on representative multi-node data, seeded by the LIVE beacon

Because the live mesh is a single empty node, a ranked/spread plan and a non-zero scoreboard
cannot come from it directly. To exercise the decision logic on real-*shaped* data we ran the
**real** modules over the apps' `web/fixtures.js` multi-vendor fabric while overriding the
fixture beacon with the **live** `GET /beacon` (live randomness, real wire shapes).

ce-sched `plan(k=3, redundancy="verify", objective="balanced")` returned a ranked, **vendor-spread**
plan (seeded by live beacon height 134822):

| replica | host | operator | region | asn | rttMs | score |
|---|---|---|---|---|---|---|
| 0 | host-us-east-beta-1 | op-beta | r0 | 64500 | 18 | 0.773 |
| 1 | host-eu-west-gamma-1 | op-gamma | r2 | 64600 | 95 | 0.682 |
| 2 | host-us-east-alpha-2 | op-alpha | r0 | 64500 | 14 | 0.778 |

- **3 distinct operators** for the 3 `verify` replicas (anti-collusion spread enforced — no
  operator gets two replicas even though op-alpha had two qualifying hosts).
- Latency-aware (rtt folded into score), reputation-aware (`trust` from `/history`).
- `host-unknown-epsilon-1` was **rejected `benchmark_suspect`** — its claimed benchmark dwarfs
  its delivered history (the cross-check from compute-fabric.md §9). The other two non-picked
  hosts were `low_score`. `shortfall: 0`, `relaxed: []`.

ce-bench `computeFabricStats` over 4 valid (ce-bench-schema) profiles + the live beacon
aggregated real capacities:

```
nodes 4 | cpu_cores 60 | cpu_gflops 1320 | gpus 2 | gpu_vram_mb 72000
gpu_tflops 202 | tokens_per_sec 3500 | storage_free_gb 850 | perf_score 205070
mesh { median_rtt_ms 18, reachable_frac 0.6, regions 3 } | by_kind {native 2, container 1, browser 1}
```

The `verified` (Sybil-gated) variant correctly collapsed to the single node with delivered
history: `nodes_verified 1, cpu_cores_verified 16, cpu_gflops_verified ~419, gpu_tflops_verified
~82, perf_score_verified ~82861` — unbonded/no-history nodes count toward display totals but
~0 toward verified, as designed. No float-money math anywhere.

---

## 3. What was empty / missing, and why

| Observation | Cause | Consequence |
|---|---|---|
| `/atlas` empty | Single-node mesh; no peers advertise capacity, and self is not listed | Placer has 0 candidates → shortfall; fabricstats nodes=0 |
| `/netgraph` 404 | Compute-fabric **P0** (libp2p ping + `/netgraph`) not present/enabled in this `--light` build | No measured RTT edges → latency ranking is neutral; mesh-health regions=0 |
| No NodeProfiles | Compute-fabric **P2** (`ce-bench` capsule + signed `NodeProfile` in atlas / `/profiles`) not deployed; `/profiles` and `/fabric/stats` are proposed routes, absent here | fabricstats compute totals are 0 on the live node |
| `/history` all zeros | Node has hosted/paid no jobs | trust signal is 0 for self |
| ce-hub down (`:8970`) | `web/ce-hub` not started | No browser nodes counted |

### Cross-app schema finding (worth flagging to the apps' authors)

`ce-sched/web/fixtures.js` builds NodeProfiles in the **older** compute-fabric §2.1 shape
(`measured_at`, `cpu`, `gpus`, … with no `schema`/`beacon_*`/`bench_app`/`runtime.kind`/`samples`,
and human-readable `node_id`s). `ce-bench/src/types.js::validateProfile` requires the **newer**
shape: `schema === 1`, `beacon_height`, `beacon_hash`, `bench_app`, `runtime.kind ∈ NODE_KINDS`,
`samples: []`, and a real **64-hex** `node_id`. As a result, feeding the ce-sched demo profiles
straight into ce-bench yields `nodes: 0` (all silently dropped by validation). The §2.3 ce-bench
numbers above used profiles authored in ce-bench's own schema. The two apps should converge on
one NodeProfile schema (the ce-bench one is the stricter superset) — or ce-sched's fixtures
should be regenerated to it — before a real `ce-bench`→atlas→`ce-sched` loop will carry profiles.

---

## 4. What is needed for a full live test

1. **A node build with compute-fabric P0** — libp2p `ping` enabled and `GET /netgraph` serving
   per-peer EWMA RTT. Without it there are no measured edges, so predicted-RTT / regions /
   latency ranking can only be exercised on fixtures.
2. **At least 2–3 peers actually connected** (laptop + desktop + relay, per CLAUDE.md) so
   `/atlas` is non-empty and `/netgraph` has edges. Two distinct operators/ASNs are the minimum
   to demonstrate vendor spread on live data; three to show a 3-replica `verify` plan.
3. **P2 deployed**: the `ce-bench` capsule running on those nodes and publishing **signed
   NodeProfiles** (via `POST /profile/publish` or the CEP-1 `/signals` stopgap), plus the node
   exposing `/profiles` (and ideally `/fabric/stats`). Then `computeFabricStats` aggregates real
   benchmarked capacity instead of fixtures.
4. **Some delivered history** — run a few jobs (`mesh-deploy`) so `/history` is non-zero and the
   trust term and the `verified` Sybil gate operate on real reputation.
5. **ce-hub running on `:8970`** (and a browser node at `ce-net.com/node`) to count
   browser/WebGPU nodes in `by_kind`.
6. **Schema convergence** (see §3) so profiles published by ce-bench validate and flow into the
   ce-sched candidate set carrying `profile.*` benchmark axes.

Until 1–3 land, the live single-node mesh validates wiring and empty-mesh behavior (done here);
the ranking/spread/aggregation logic is validated against representative data seeded by the live
beacon (also done here).

---

## 5. How to reproduce

```bash
# raw probe
curl -s http://localhost:8844/status
curl -s http://localhost:8844/beacon
curl -s http://localhost:8844/atlas
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8844/netgraph   # 404 on a pre-P0 node
curl -s http://localhost:8844/history/<node_id_from_status>

# offline self-tests
cd ce-sched && node -e "import('./src/placer.js').then(m=>console.log(m.__selftest()))"
cd ce-bench && node -e "import('./src/fabricstats.js').then(async m=>{m.__selftest();console.log(await m.__selftestAsync())})"
```

The two live harnesses used for §2.2/§2.3 were scratch scripts outside the repos (they import
the apps' real `src/` modules unchanged); rebuild them by injecting a tolerant fetch that maps a
`/netgraph` 404 to `[]` and, for §2.3, overriding the fixture beacon with `GET /beacon`.
