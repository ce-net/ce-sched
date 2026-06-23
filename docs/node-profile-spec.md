# Design spec: signed NodeProfile + ce-bench capsule (for the node / ce-bench team)

**This is a specification, not an implementation.** `ce-sched` is an app and must not change the
`ce` node or `ce-fabric`. This document records exactly what `ce-sched` (P3 placement) needs the
node + `ce-bench` (P2) to publish, so the node team can implement it. It restates and tightens
`ce/docs/compute-fabric.md` §2.1 / §4 from the *consumer's* point of view.

## What ce-sched needs

1. A **signed `NodeProfile`** per node, carrying measured compute capability (not self-tags).
2. That profile **readable from `/atlas`** (folded in, like capacity today) or a sibling endpoint —
   so the existing single-fetch placement path picks it up with no new auth.
3. **Trust hooks**: a `measured_at` inside the signature (no backdating) and enough on-wire context
   to run the §9 cross-check (claimed throughput vs delivered work in `/history`).

## NodeProfile (matches compute-fabric.md §2.1, with consumer notes)

```jsonc
{
  "node_id": "<64 hex>",
  "measured_at": 1716470400,            // unix secs, INSIDE the signed bytes — anti-backdate
  "cpu":     { "cores": 16, "threads": 32, "gflops_fp32": 540.0, "mem_bw_gbps": 51.2 },
  "gpus":    [ { "model": "RTX 4090", "backend": "Cuda", "vram_mb": 24576, "fp16_tflops": 165.0 } ],
  "memory":  { "total_mb": 65536, "available_mb": 48000 },
  "storage": { "total_gb": 2000, "free_gb": 1200, "read_mbps": 3200, "write_mbps": 2800 },
  "llm":     { "ref_model": "llama-3.1-8b-q4", "tokens_per_sec": 92.0 },
  "runtime": { "os": "linux", "arch": "x86_64", "docker": true, "gvisor": true, "wasm": true },
  "bench":   { "capsule": "ce-bench", "version": "0.1.0", "beacon_height": 1180 }, // provenance
  "sig":     "<128 hex>"                // Ed25519 over the canonical bytes of all fields above
}
```

### Consumer requirements (what ce-sched relies on)

- **Stable axis names.** `ce-sched/src/scorer.js` reads exactly these axes:
  `cpu.gflops_fp32`, `cpu.mem_bw_gbps`, `gpus[].vram_mb`, `gpus[].fp16_tflops`, `llm.tokens_per_sec`,
  `storage.read_mbps`, `storage.write_mbps`, `memory.available_mb`. Renaming breaks the scorer.
- **A measured vector, never a single scalar.** The display `perf_score` (compute-fabric.md §2.4) is
  fine for the scoreboard, but placement needs the *vector* — keep all axes on the wire.
- **`measured_at` must be inside the signature** and reasonably fresh; the scorer down-weights stale
  profiles (`now - measured_at > staleSecs`).
- **`bench.beacon_height`** (the beacon the capsule used to randomize its run window) lets the
  consumer confirm the measurement was taken under a beacon-seeded schedule (anti "benchmark mode"
  detection, §9). Optional but recommended.
- **Camel/snake:** keep snake_case on the wire (matches `/atlas`/`/netgraph`); `ce-sched` normalizes.

## Trust / adversarial hooks ce-sched will use (no node work beyond exposing data)

`ce-sched` does the §9 cross-checks at the app layer from public data — the node only needs to make
the data readable:

- **Claim-vs-delivered:** compare `gpus[].fp16_tflops` / `llm.tokens_per_sec` against `/history`
  `jobs_hosted` + `earned`. A high claim with near-zero delivered work ⇒ `benchmarkSuspect`, trust
  floored. Needs: profile readable + `/history` (already exists).
- **Co-signed edges / Vivaldi error** are already the node's job (compute-fabric.md §9); `ce-sched`
  just consumes `/netgraph`.

## Endpoint shape requested

Either of these works for `ce-sched` (prefer the first — zero new auth, one fetch):

1. **Fold into `/atlas`** — add an optional `profile` object to each atlas entry:
   `{ node_id, cpu_cores, mem_mb, running_jobs, last_seen_secs, tags, profile?: NodeProfile }`.
   `ce-sched` already fetches `/atlas`; `profile` is read opportunistically and ignored if absent.
2. **`GET /fabric/profiles`** — array of `NodeProfile`. Acceptable; costs `ce-sched` one extra fetch.

Until either ships, `ce-sched` runs on the atlas fallback (cores/mem/tags → coarse axis estimates,
`benchFit.source = "atlas"`, confidence < 1). The placement contract does not change when profiles
arrive — only the data source upgrades and confidence rises to 1.

## Out of scope for this spec (node team owns)

- The `ce-bench` capsule itself (WASM CPU/mem/disk probes + native GPU probe), its beacon-seeded
  scheduling, and gossiping the signed profile into the atlas — that is the P2 deliverable in
  `compute-fabric.md` §4, owned by the node/`ce-bench` team. `ce-sched` is purely a consumer.
