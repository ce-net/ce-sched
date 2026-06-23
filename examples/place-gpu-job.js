/**
 * Example: pick 3 vendor-diverse, low-latency GPU hosts with K=2-of redundancy verification for a
 * LOW-TRUST scenario, using @ce-net/sched's `planPlacement` facade.
 *
 * The scenario: we want to run a GPU job (needs VRAM + a measured profile when available) but we do
 * NOT trust any single host — so we ask for redundancy="verify" (independent replicas, cross-checked)
 * and we spread across distinct operators / ASNs / regions so no one vendor (or correlated cluster)
 * can collude or fail together. The job is placed on the lowest-latency GPU hosts that satisfy those
 * independence constraints, with the final tie-break seeded by /beacon so a host cannot steer whether
 * it is chosen.
 *
 * Run against a live node:   node examples/place-gpu-job.js  [http://localhost:8844]
 * Run offline (mocked):      node examples/place-gpu-job.js  --mock
 *
 * The mock path uses a synthetic Fabric Map (4 GPU operators across 3 regions/ASNs, mixed trust) so
 * the example is self-verifying with no node running — that is also how the workflow exercises it.
 *
 * NOTE: this prints a PlacementPlan (advice + provenance). Dispatch is a separate, explicit step —
 * loop `plan.targets` and call `ce.meshDeploy({...})` (commented at the bottom). ce-sched never
 * deploys for you.
 */

import { planPlacement } from "../src/index.js";

/* ------------------------------------------------------------------------------------------------ *
 * The job: a GPU workload. We declare a demand VECTOR (not a scalar) so benchFit matches real
 * hardware: VRAM dominates, with some fp16 TFLOPS and a token-throughput floor for an inference job.
 * cpuCores/memMb are HARD per-host requirements; requireTags gates on advertised capabilities.
 * ------------------------------------------------------------------------------------------------ */
const jobSpec = {
  k: 3, // we want 3 hosts...
  cpuCores: 2,
  memMb: 4096,
  requireTags: ["docker", "gpu"],

  // Low-trust posture: verify across INDEPENDENT replicas (the placer forces distinct operators and
  // raises effectiveK based on the best feasible trust). maxShare caps any one correlation group:
  // with k=3, maxShare=0.33 -> perGroupCap = ceil(3*0.33) = 1, so NO two replicas may share an
  // operator, ASN, OR region. That hard-excludes the correlated us-a/us-b pair (same ASN+region)
  // from both being chosen — exactly the risk-spreading a low-trust job wants. (Bump maxShare toward
  // 0.5+ to tolerate some co-location when independent hosts are scarce; the plan then reports the
  // relaxed constraints instead of silently violating them.)
  redundancy: "verify",
  maxShare: 0.33,

  // Throughput objective: weight benchFit (real GPU capability) heavily; still reward low RTT.
  objective: "throughput",

  // Benchmark demand vector (consumed from signed NodeProfiles when present; atlas-estimated
  // otherwise, at reduced confidence). Each axis saturates at its `target` ("enough") bar.
  demand: {
    vramMb: { weight: 3, target: 16000 }, // a 16 GB card is "enough"
    fp16Tflops: { weight: 1, target: 40 },
    tokensPerSec: { weight: 1, target: 60 },
  },

  // Capability floor: prefer hosts proven to clear 12 GB VRAM. requireProfile=false => unprofiled
  // hosts are NOT excluded (they survive at discounted trust) so a young network still places.
  minVramMb: 12000,
  requireProfile: false,

  // Deterministic, auditable selection seeded by /beacon; a per-request nonce keeps two identical
  // jobs from landing on exactly the same hosts.
  selection: "best",
  nonce: "place-gpu-job-example",
};

/* ------------------------------------------------------------------------------------------------ *
 * A self-contained mock CE client: a synthetic low-trust GPU fleet. Four GPU operators:
 *   us-a, us-b  -> region US,  ASN 64500 (CORRELATED: same provider + same latency region)
 *   eu-a        -> region EU,  ASN 64600
 *   ap-a        -> region AP,  ASN 64700 (far, but fully independent)
 * Mixed trust via /history: us-a is a veteran; the rest are fresh/low-trust. Because we demand
 * vendor diversity + verify, the plan must NOT take both us-a and us-b (they are correlated) — it
 * spreads across the independent operators even though us-b is closer than eu-a/ap-a.
 * ------------------------------------------------------------------------------------------------ */
function mockClient() {
  const now = Math.floor(Date.now() / 1000);
  const profile = (nodeId, vram, tflops, tps) => ({
    node_id: nodeId,
    measured_at: now - 30,
    cpu: { cores: 16, threads: 32, gflops_fp32: 800, mem_bw_gbps: 60 },
    gpus: [{ model: "synthetic", backend: "Cuda", vram_mb: vram, fp16_tflops: tflops }],
    memory: { total_mb: 64000, available_mb: 48000 },
    storage: { total_gb: 2000, free_gb: 1200, read_mbps: 3000, write_mbps: 2000 },
    llm: { ref_model: "ce-ref-tiny", tokens_per_sec: tps },
    runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: true, wasm: true },
  });
  const atlas = [
    { node_id: "us-a", cpu_cores: 16, mem_mb: 64000, running_jobs: 1, last_seen_secs: now - 5, tags: ["docker", "gpu", "asn:64500"], profile: profile("us-a", 24000, 80, 120) },
    { node_id: "us-b", cpu_cores: 16, mem_mb: 64000, running_jobs: 0, last_seen_secs: now - 5, tags: ["docker", "gpu", "asn:64500"], profile: profile("us-b", 16000, 45, 70) },
    { node_id: "eu-a", cpu_cores: 16, mem_mb: 64000, running_jobs: 0, last_seen_secs: now - 5, tags: ["docker", "gpu", "asn:64600"], profile: profile("eu-a", 24000, 80, 120) },
    { node_id: "ap-a", cpu_cores: 16, mem_mb: 64000, running_jobs: 0, last_seen_secs: now - 5, tags: ["docker", "gpu", "asn:64700"], profile: profile("ap-a", 16000, 45, 70) },
    // a non-GPU host (filtered by requireTags) and an undersized one (filtered by headroom):
    { node_id: "cpu-x", cpu_cores: 32, mem_mb: 128000, running_jobs: 0, last_seen_secs: now - 5, tags: ["docker"] },
    { node_id: "tiny", cpu_cores: 1, mem_mb: 512, running_jobs: 0, last_seen_secs: now - 5, tags: ["docker", "gpu", "asn:64800"] },
  ];
  const netgraph = [
    { peer: "us-a", rtt_ms: 6, samples: 12, last_seen_secs: now },
    { peer: "us-b", rtt_ms: 8, samples: 12, last_seen_secs: now }, // closest independent? no — same group as us-a
    { peer: "eu-a", rtt_ms: 85, samples: 9, last_seen_secs: now },
    { peer: "ap-a", rtt_ms: 165, samples: 7, last_seen_secs: now },
    { peer: "cpu-x", rtt_ms: 4, samples: 12, last_seen_secs: now },
    { peer: "tiny", rtt_ms: 12, samples: 8, last_seen_secs: now },
  ];
  const histories = new Map([
    ["us-a", { jobs_hosted: 300, heartbeats_hosted: 6000, earned: "9000000000000000000000", recent_earned: "1500000000000000000000" }],
    ["us-b", { jobs_hosted: 2, heartbeats_hosted: 20, earned: "30000000000000000000", recent_earned: "30000000000000000000" }],
    ["eu-a", { jobs_hosted: 1, heartbeats_hosted: 10, earned: "10000000000000000000", recent_earned: "10000000000000000000" }],
    ["ap-a", { jobs_hosted: 0, heartbeats_hosted: 0, earned: "0", recent_earned: "0" }],
  ]);
  return {
    status: async () => ({ node_id: "me", height: 12345, balance: "0" }),
    netgraph: async () => netgraph,
    atlas: async () => atlas,
    histories: async (ids) => new Map(ids.map((id) => [id, histories.get(id) ?? null])),
    beacon: async () => ({ height: 88888, hash: "a1b2c3d4e5f600112233445566778899" }),
  };
}

/* ------------------------------------------------------------------------------------------------ *
 * Run it.
 * ------------------------------------------------------------------------------------------------ */
async function main() {
  const args = process.argv.slice(2);
  const mock = args.includes("--mock") || args.includes("-m");
  const baseUrl = args.find((a) => a.startsWith("http")) || "http://localhost:8844";

  const ceOpt = mock ? mockClient() : baseUrl;
  if (!mock) {
    console.log(`Planning against live node ${baseUrl} (use --mock for the offline synthetic fleet)\n`);
  } else {
    console.log("Planning against the synthetic low-trust GPU fleet (--mock)\n");
  }

  const plan = await planPlacement(jobSpec, { ce: ceOpt });

  // --- report -----------------------------------------------------------------------------------
  console.log(`objective=${plan.objective}  requestedK=${plan.requestedK}  effectiveK=${plan.effectiveK}` + (plan.shortfall ? `  shortfall=${plan.shortfall}` : ""));
  console.log(`beacon: height=${plan.beacon.height} hash=${plan.beacon.hash.slice(0, 12)}…`);
  console.log(`weights: L=${plan.weights.wL.toFixed(2)} B=${plan.weights.wB.toFixed(2)} T=${plan.weights.wT.toFixed(2)} P=${plan.weights.wP.toFixed(2)} D=${plan.weights.wD.toFixed(2)}`);
  if (plan.relaxed.length) console.log(`relaxed constraints: ${plan.relaxed.join(", ")}`);
  console.log("\nchosen targets (vendor-diverse, low-latency, redundant):");
  for (const t of plan.targets) {
    console.log(
      `  #${t.replica}  ${t.nodeId.padEnd(8)}  score=${t.score.toFixed(3)}  rtt=${Math.round(t.rttMs)}ms  ` +
        `benchFit=${t.benchFit.toFixed(2)}  trust=${t.trust.toFixed(2)}  ` +
        `[op=${t.groups.operator} asn=${t.groups.asn} region=${t.groups.region}]`,
    );
  }
  if (plan.rejected.length) {
    console.log("\nrejected:");
    for (const r of plan.rejected) console.log(`  ${r.nodeId.padEnd(8)}  ${r.reason}`);
  }

  // --- independence / diversity assertions (self-check on the mock fleet) ------------------------
  if (mock) {
    const ops = plan.targets.map((t) => t.groups.operator);
    const asns = plan.targets.map((t) => t.groups.asn);
    const regions = plan.targets.map((t) => t.groups.region);
    const distinctOps = new Set(ops).size === ops.length;
    const distinctAsns = new Set(asns).size === asns.length;
    const distinctRegions = new Set(regions).size === regions.length;
    const tookBothCorrelated = ops.includes("us-a") && ops.includes("us-b");
    console.log("\nself-check:");
    console.log(`  3 hosts placed:           ${plan.targets.length === 3}`);
    console.log(`  distinct operators:       ${distinctOps}`);
    console.log(`  distinct ASNs:            ${distinctAsns}`);
    console.log(`  distinct regions:         ${distinctRegions}`);
    console.log(`  avoided correlated pair:  ${!tookBothCorrelated} (never both us-a & us-b)`);
    if (plan.targets.length !== 3) throw new Error("FAIL: expected 3 GPU hosts");
    if (!distinctOps) throw new Error("FAIL: replicas share an operator (verify requires independence)");
    if (tookBothCorrelated) throw new Error("FAIL: took both correlated us-a & us-b");
    console.log("\nplace-gpu-job example OK");
  }

  // --- dispatch (commented): the explicit, separate step the caller owns -------------------------
  //   import { CeClient } from "../src/index.js";
  //   const ce = new CeClient(baseUrl);
  //   for (const t of plan.targets) {
  //     const { job_id } = await ce.meshDeploy({
  //       node_id: t.nodeId, image: "ghcr.io/me/gpu-job:latest", cmd: ["./run"],
  //       cpu_cores: jobSpec.cpuCores, mem_mb: jobSpec.memMb, duration_secs: 3600,
  //       bid: "1000000000000000000", // base-unit STRING, never a float
  //     });
  //     console.log(`dispatched replica ${t.replica} to ${t.nodeId} -> job ${job_id}`);
  //   }
}

main().catch((err) => {
  console.error("place-gpu-job failed:", err && err.message ? err.message : err);
  process.exit(1);
});
