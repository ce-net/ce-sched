# ce-sched daemon — HTTP API contract

The ce-sched daemon (`ce-sched-daemon`) is a **per-machine singleton agent** (one per node, supervised
by ce-appmgr), the placement counterpart to the ce-bench daemon. It binds a fixed local port so every
SDK on the machine finds it deterministically.

- **Base URL:** `http://127.0.0.1:8856` (override the bind with `$CE_SCHED_ADDR`)
- **Upstreams it reads:**
  - the local **ce-bench** daemon at `http://127.0.0.1:8855` (override `$CE_BENCH_ADDR`) — the
    assembled Fabric Map (per-node signed `NodeProfile`s + folded mesh gossip + `FabricStats`);
  - the local **CE node** HTTP API at `http://127.0.0.1:8844` (via `ce-rs`) — the read-substrate
    (`/atlas`, `/netgraph`, `/status`) used as a fallback map source, and `/mesh-deploy` for dispatch.
- **Content type:** `application/json` (request + response).
- **Money:** integer base units as **decimal strings** (`1 credit = 10^18` base units). Never parse a
  `*BaseUnits` field to a float.
- **Casing:** the placement vocabulary (`JobSpec`, `PlacementPlan`, …) is **camelCase**; the
  `FabricMap` family mirrors ce-bench and is **snake_case**.

The wire types are defined once in `crates/ce-sched-core/src/api.rs` and consumed unchanged by the
Rust SDK (`ce-sched-sdk`) and the TS/JS SDK (`sdk-ts`).

---

## `GET /health`

Liveness + the configured upstreams.

**200** →
```json
{
  "status": "ok",
  "service": "ce-sched-daemon",
  "version": "0.0.1",
  "node_api": "http://127.0.0.1:8844",
  "bench_api": "http://127.0.0.1:8855"
}
```

---

## `GET /map`

The assembled **Fabric Map** the planner scores hosts against. Sourced from the local ce-bench daemon
when available; otherwise assembled from the node read-substrate (capacity + edges only, no measured
profiles — planning degrades gracefully).

**200** → `FabricMap`
```json
{
  "nodes": [
    {
      "node_id": "us-a",
      "capacity": { "node_id": "us-a", "cpu_cores": 8, "mem_mb": 16000, "running_jobs": 0,
                    "last_seen_secs": 10, "tags": ["docker", "gpu"], "ask_base_units": null },
      "profile": {
        "node_id": "us-a", "measured_at": 1719600000,
        "cpu": { "cores": 8, "threads": 16, "gflops_fp32": 400, "mem_bw_gbps": 50 },
        "gpus": [ { "model": "X", "backend": "Cuda", "vram_mb": 24000, "fp16_tflops": 80 } ],
        "memory": { "total_mb": 16000, "available_mb": 12000 },
        "storage": { "total_gb": 1000, "free_gb": 500, "read_mbps": 2000, "write_mbps": 1500 },
        "llm": { "ref_model": "m", "tokens_per_sec": 120 },
        "runtime": { "os": "linux", "arch": "x86_64", "docker": true, "gvisor": true, "wasm": true, "kind": "Native" },
        "sig": "<128 hex>"
      }
    }
  ],
  "edges": [ { "a": "us-a", "b": "us-b", "rtt_ms": 5.3, "samples": 12, "last_seen_secs": 1719600000 } ],
  "origin": "us-a",
  "stats": {
    "nodes": 1, "cpu_cores": 8, "cpu_gflops": 400, "gpus": 1, "gpu_vram_mb": 24000, "gpu_tflops": 80,
    "tokens_per_sec": 120, "storage_free_gb": 500, "perf_score": 1.0,
    "mesh": { "median_rtt_ms": 5.3, "reachable_frac": 1.0, "regions": 1 },
    "by_kind": { "native": 1, "container": 0, "browser": 0 },
    "computed_at": 1719600000
  },
  "assembled_at_ms": 1719600000123
}
```

---

## `POST /plan`

`JobSpec → PlacementPlan`. The daemon auto-fills `payer` (the latency origin) from the local node's
`/status` when omitted, assembles the map, and runs the **pure planner** (`ce-sched-core::placer::plan`).

**Request body — `JobSpec`** (only `cpuCores` + `memMb` are required):
```json
{
  "payer": "me",
  "k": 3,
  "cpuCores": 1,
  "memMb": 256,
  "requireTags": ["docker", "gpu"],
  "exclude": [],
  "allowSelf": false,
  "maxStaleSecs": 180,
  "objective": "balanced",
  "weights": { "wL": 0.25, "wB": 0.25, "wT": 0.25, "wP": 0.1, "wD": 0.15 },
  "demand": { "vramMb": { "weight": 1, "target": 8000 } },
  "minGflops": 100, "minVramMb": 8000, "minTokensPerSec": 20,
  "requireProfile": false,
  "rttSoftCapMs": 250,
  "trustSaturation": 50,
  "priceCapBaseUnits": "1000000000000000000000",
  "defaultPriceScore": 0.5,
  "redundancy": "verify",
  "maxShare": 0.34,
  "recentPlacements": { "eu-a": 8, "us-a": 2 },
  "cohort": "spread",
  "selection": "best",
  "temperature": 0.15,
  "tieEps": 0.02,
  "beaconDepth": 0,
  "nonce": "job-1",
  "refine": false
}
```
`redundancy` is `"none" | "verify" | <number in (0,1)>` (target confidence). `objective` is
`latency | throughput | balanced | cheap`. `cohort` is `spread | colocate | dag`. `selection` is
`best | weighted`.

**200** → `PlacementPlan`
```json
{
  "targets": [
    { "nodeId": "us-a", "score": 0.87, "rttMs": 5, "benchFit": 0.9, "trust": 0.4,
      "groups": { "operator": "us-a", "asn": "64500", "region": "r0", "cluster": "us-a|64500|r0" },
      "replica": 0 }
  ],
  "effectiveK": 2,
  "requestedK": 1,
  "beacon": { "height": 999, "hash": "deadbeef..." },
  "weights": { "wL": 0.5, "wB": 0.15, "wT": 0.2, "wP": 0.05, "wD": 0.1 },
  "objective": "latency",
  "relaxed": [],
  "shortfall": 0,
  "rejected": [ { "nodeId": "eu-a", "reason": "low_score" } ],
  "assembledAtMs": 1719600000123
}
```

**Errors**
- **400** `{ "error": "plan: spec.payer ... is required" }` — payer unset and unresolvable.
- **501** `{ "error": "plan: pure planner port not yet implemented" }` — while the planner port is a
  scaffold stub (it validates the precondition and returns 501; the JS `src/placer.js` remains the
  working implementation until the Rust port lands).
- **502** `{ "error": "node /atlas unavailable: ..." }` — the read-substrate could not be gathered.

The plan is **advice + provenance, not an action**: it is deterministic and replayable given the same
`beacon` + `nonce` + candidate set (the §6 auditability guarantee). Dispatch is a separate call.

---

## `POST /place`

Dispatch a plan's targets. For each `PlanTarget` the daemon calls the node's `/mesh-deploy` with the
supplied `DeploySpec`; a failed dispatch does not abort the rest.

**Request body — `PlaceRequest`**:
```json
{
  "plan": { "...": "a PlacementPlan from POST /plan" },
  "deploy": {
    "image": "alpine:latest",
    "cmd": ["echo", "hi"],
    "cpuCores": 1,
    "memMb": 256,
    "durationSecs": 60,
    "bid": "10000000000000000000",
    "grant": "<optional capability token>"
  }
}
```

**200** → `DispatchResult`
```json
{
  "dispatched": [
    { "nodeId": "us-a", "replica": 0, "ok": true, "jobId": "job-abc" },
    { "nodeId": "eu-a", "replica": 1, "ok": false, "error": "402 insufficient balance" }
  ],
  "placed": 1,
  "assembledAtMs": 1719600000456
}
```

**Errors**
- **400** `{ "error": "invalid bid base units: ..." }` — `deploy.bid` is not a base-unit integer string.

---

## Architecture notes

- **One daemon per machine.** A second install is a no-op/refresh, never a second process. No
  capability placement — ce-sched installs on every participating node and serves its local API.
- **The planner is pure.** All policy lives in `ce-sched-core` as `(JobSpec, FabricMap) → PlacementPlan`
  with no I/O, so it is fully unit-testable without a node and the SDKs can plan **without the daemon
  hop** (`ce-sched-sdk` re-exports `plan_pure`; call it with a `FabricMap` you already hold).
- **ce-bench is the map source.** ce-sched consumes what ce-bench measures and gossips; it never
  re-implements measurement. See `../ce-bench/docs/RUST-DAEMON-ARCH.md`.
