/**
 * Synthetic fabric fixtures for the placement visualizer.
 *
 * The visualizer must be usable with NO live CE node (offline / disk-constrained dev box). These
 * fixtures imitate the exact wire shapes `CeClient` returns (`/netgraph`, `/atlas`, `/beacon`,
 * `/history/:id`) so the demo drives the SAME real pipeline (buildGraph -> feasible -> tagCandidates
 * -> select) as a live node would. snake_case on the wire, base-unit STRINGS for money.
 *
 * The scenario is deliberately varied so the visualizer's points all light up:
 *   - two operators sharing one ASN/region (correlation -> diversity cap should bite),
 *   - a far, cheap, well-benchmarked host (latency vs price/bench tension),
 *   - a host whose CLAIMED benchmark dwarfs its delivered history (benchmark_suspect cross-check),
 *   - a stale host (liveness reject) and an underpowered host (headroom reject),
 *   - a profile-less host (atlas fallback, confidence < 1).
 *
 * @module web/fixtures
 */

/** Base unit scale: 1 credit = 10^18 base units (see CLAUDE.md money model). */
const CREDIT = 10n ** 18n;
/** Format a credits-per-hour figure as a base-unit string (integer base units only). */
function askStr(creditsPerHourTimes1000) {
  // creditsPerHourTimes1000 is milli-credits/hr to keep callers integer; -> base units.
  return ((BigInt(creditsPerHourTimes1000) * CREDIT) / 1000n).toString();
}

const PAYER = "payer000000000000000000000000000000000000000000000000000000000000";

/** Profile builder. Mirrors the consumer NodeProfile in src/types.js (snake_case wire form). */
function profile(nodeId, { gflops, memBw, vram = 0, tflops = 0, tokens = 0, read = 500, write = 400, gpuModel = "" }) {
  return {
    node_id: nodeId,
    measured_at: 1_900_000_000,
    cpu: { cores: 8, threads: 16, gflops_fp32: gflops, mem_bw_gbps: memBw },
    gpus: vram > 0 ? [{ model: gpuModel, backend: "Cuda", vram_mb: vram, fp16_tflops: tflops }] : [],
    memory: { total_mb: 16000, available_mb: 14000 },
    storage: { total_gb: 512, free_gb: 300, read_mbps: read, write_mbps: write },
    llm: { ref_model: "llama-3-8b", tokens_per_sec: tokens },
    runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: true, wasm: true },
    sig: "demo",
  };
}

/**
 * Returns a self-contained fake client implementing the CeClient read surface the placer uses.
 * `nowSecs` anchors liveness so the "stale" host is reliably stale.
 * @param {number} nowSecs
 */
export function demoClient(nowSecs = Math.floor(Date.now() / 1000)) {
  // ---- /atlas (capacity + optional signed profile) -----------------------------------------
  const atlas = [
    // Operator alpha, ASN 64500, US-east region (us-a and us-b are correlated).
    {
      node_id: "host-us-east-alpha-1".padEnd(64, "0"),
      cpu_cores: 16, mem_mb: 32000, running_jobs: 1, last_seen_secs: nowSecs - 5,
      tags: ["docker", "gpu", "asn:64500", "region:us-east", "op:alpha"],
      profile: profile("host-us-east-alpha-1".padEnd(64, "0"), { gflops: 420, memBw: 48, vram: 24000, tflops: 82, tokens: 1400, gpuModel: "RTX-4090" }),
      ask_base_units: askStr(120),
    },
    {
      node_id: "host-us-east-alpha-2".padEnd(64, "0"),
      cpu_cores: 16, mem_mb: 32000, running_jobs: 6, last_seen_secs: nowSecs - 9,
      tags: ["docker", "gpu", "asn:64500", "region:us-east", "op:alpha"],
      profile: profile("host-us-east-alpha-2".padEnd(64, "0"), { gflops: 410, memBw: 47, vram: 24000, tflops: 80, tokens: 1350, gpuModel: "RTX-4090" }),
      ask_base_units: askStr(125),
    },
    // Operator beta, same ASN/region as alpha -> correlated outage group with alpha.
    {
      node_id: "host-us-east-beta-1".padEnd(64, "0"),
      cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: nowSecs - 4,
      tags: ["docker", "asn:64500", "region:us-east", "op:beta"],
      profile: profile("host-us-east-beta-1".padEnd(64, "0"), { gflops: 300, memBw: 38 }),
      ask_base_units: askStr(90),
    },
    // Operator gamma, EU, far but cheap and strong on bench.
    {
      node_id: "host-eu-west-gamma-1".padEnd(64, "0"),
      cpu_cores: 32, mem_mb: 64000, running_jobs: 2, last_seen_secs: nowSecs - 7,
      tags: ["docker", "gpu", "asn:64600", "region:eu-west", "op:gamma"],
      profile: profile("host-eu-west-gamma-1".padEnd(64, "0"), { gflops: 560, memBw: 60, vram: 48000, tflops: 120, tokens: 2100, gpuModel: "A100" }),
      ask_base_units: askStr(70),
    },
    // Operator delta, APAC, profile-less (atlas fallback, confidence < 1).
    {
      node_id: "host-ap-south-delta-1".padEnd(64, "0"),
      cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: nowSecs - 6,
      tags: ["docker", "asn:64700", "region:ap-south", "op:delta"],
      ask_base_units: askStr(60),
    },
    // Operator epsilon: CLAIMS huge bench but has essentially no delivered history -> suspect.
    {
      node_id: "host-unknown-epsilon-1".padEnd(64, "0"),
      cpu_cores: 64, mem_mb: 128000, running_jobs: 0, last_seen_secs: nowSecs - 3,
      tags: ["docker", "gpu", "asn:64800", "region:us-west", "op:epsilon"],
      profile: profile("host-unknown-epsilon-1".padEnd(64, "0"), { gflops: 980, memBw: 90, vram: 80000, tflops: 300, tokens: 5000, gpuModel: "H100x?" }),
      ask_base_units: askStr(40),
    },
    // Stale host (liveness reject).
    {
      node_id: "host-stale-zeta-1".padEnd(64, "0"),
      cpu_cores: 16, mem_mb: 32000, running_jobs: 0, last_seen_secs: nowSecs - 4000,
      tags: ["docker", "asn:64900", "region:us-east", "op:zeta"],
      profile: profile("host-stale-zeta-1".padEnd(64, "0"), { gflops: 400, memBw: 44 }),
      ask_base_units: askStr(100),
    },
    // Underpowered host (headroom reject for a 4-core ask).
    {
      node_id: "host-tiny-eta-1".padEnd(64, "0"),
      cpu_cores: 2, mem_mb: 2000, running_jobs: 1, last_seen_secs: nowSecs - 2,
      tags: ["docker", "asn:64950", "region:eu-west", "op:eta"],
      ask_base_units: askStr(20),
    },
  ];

  // ---- /netgraph (measured RTT from the PAYER's vantage) --------------------------------------
  // Far hosts (eu/ap/us-west) have no direct measured edge -> predicted via embedding.
  const netgraph = [
    { peer: "host-us-east-alpha-1".padEnd(64, "0"), rtt_ms: 12, samples: 40, last_seen_secs: nowSecs - 5 },
    { peer: "host-us-east-alpha-2".padEnd(64, "0"), rtt_ms: 14, samples: 35, last_seen_secs: nowSecs - 9 },
    { peer: "host-us-east-beta-1".padEnd(64, "0"), rtt_ms: 18, samples: 22, last_seen_secs: nowSecs - 4 },
    { peer: "host-eu-west-gamma-1".padEnd(64, "0"), rtt_ms: 95, samples: 18, last_seen_secs: nowSecs - 7 },
    { peer: "host-unknown-epsilon-1".padEnd(64, "0"), rtt_ms: 70, samples: 10, last_seen_secs: nowSecs - 3 },
    { peer: "host-ap-south-delta-1".padEnd(64, "0"), rtt_ms: 180, samples: 9, last_seen_secs: nowSecs - 6 },
  ];

  // ---- /history (reputation substrate; amounts as base-unit strings) --------------------------
  // `owner` collapses two nodes to ONE operator group (verification across them proves nothing):
  // alpha-1 and alpha-2 share owner "op-alpha" -> verify mode must NOT pick both.
  const histories = new Map([
    ["host-us-east-alpha-1".padEnd(64, "0"), { owner: "op-alpha", jobs_hosted: 320, heartbeats_hosted: 5400, slashes: 0, credits_earned_base_units: askStr(50000) }],
    ["host-us-east-alpha-2".padEnd(64, "0"), { owner: "op-alpha", jobs_hosted: 280, heartbeats_hosted: 4900, slashes: 0, credits_earned_base_units: askStr(44000) }],
    ["host-us-east-beta-1".padEnd(64, "0"), { owner: "op-beta", jobs_hosted: 60, heartbeats_hosted: 700, slashes: 0, credits_earned_base_units: askStr(6000) }],
    ["host-eu-west-gamma-1".padEnd(64, "0"), { owner: "op-gamma", jobs_hosted: 150, heartbeats_hosted: 2600, slashes: 0, credits_earned_base_units: askStr(20000) }],
    ["host-ap-south-delta-1".padEnd(64, "0"), { owner: "op-delta", jobs_hosted: 12, heartbeats_hosted: 90, slashes: 0, credits_earned_base_units: askStr(800) }],
    ["host-unknown-epsilon-1".padEnd(64, "0"), { owner: "op-epsilon", jobs_hosted: 1, heartbeats_hosted: 2, slashes: 0, credits_earned_base_units: askStr(10) }],
  ]);

  return {
    payer: PAYER,
    async netgraph() { return netgraph; },
    async atlas() { return atlas; },
    async beacon() { return { height: 482190, hash: "00000000a91f3c7d2b6e4f08c1d59a7e3b0f8c2d6a14e97b5c0d3f8a2e1b4c6d" }; },
    async history(id) { return histories.get(id) ?? null; },
    async histories(ids) {
      const out = new Map();
      for (const id of ids) out.set(id, histories.get(id) ?? null);
      return out;
    },
  };
}

export { PAYER as DEMO_PAYER };
