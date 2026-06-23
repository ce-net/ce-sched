/**
 * @ce-net/sched — smart, latency- and benchmark-aware, BFT-safe job placement on the CE fabric.
 *
 * Public entry. Re-exports the client, the placement vocabulary, and the four placement modules,
 * plus the one-call facade `planPlacement`. The placement brain is `plan()` from ./placer.js;
 * everything else is the substrate it composes.
 *
 * @example  // one-call facade (resolves the payer from /status, builds a graph, returns a plan)
 * import { planPlacement } from "@ce-net/sched";
 * const plan = await planPlacement(
 *   { k: 3, cpuCores: 1, memMb: 256, requireTags: ["docker"], redundancy: "verify" },
 *   { ce: "http://localhost:8844" },
 * );
 * // plan.targets -> dispatch each with ce.meshDeploy(...)
 *
 * @example  // low-level: bring your own client + explicit payer
 * import { CeClient, plan } from "@ce-net/sched";
 * const ce = new CeClient("http://localhost:8844");
 * const result = await plan({ payer: myNodeId, k: 3, cpuCores: 1, memMb: 256,
 *                             requireTags: ["docker"], redundancy: "verify" }, ce);
 *
 * @module index
 */

import { CeClient } from "./ce.js";
import { plan } from "./placer.js";

export { CeClient, ceClient } from "./ce.js";
export {
  OBJECTIVE_WEIGHTS,
  REQUEST_DEFAULTS,
  clamp01,
  withDefaults,
} from "./types.js";

export { buildGraph, Graph } from "./graph.js";
export { resolveWeights, staticScore, latencyScore, benchFitScore, trustScore, priceScore } from "./scorer.js";
export { groupKeys, tagCandidates, perGroupCap, diversityPenalty, violatesCap, clusterOf } from "./vendor.js";
export { plan, feasible, redundancyFor, select, beaconSeed } from "./placer.js";

/** @typedef {import("./types.js").PlacementRequest} PlacementRequest */
/** @typedef {import("./types.js").PlacementPlan} PlacementPlan */

/**
 * One-call placement facade: resolve the CE client, fill in the payer (the latency origin) from the
 * node's `/status` when the caller didn't supply one, and return a {@link PlacementPlan}. This is the
 * convenience entry the task spec asks for — `plan()` remains the fully-injected low-level form.
 *
 * `jobSpec` is a {@link PlacementRequest}; `payer` is optional here (auto-resolved). `opts.ce` may be
 * a ready CeClient, a base-URL string, or omitted (defaults to http://localhost:8844). Every other
 * `plan()` option (`now`, `embedding`, `scoreFn`) passes through unchanged.
 *
 * `opts.ce` may be a ready CeClient, a base-URL string, a duck-typed client object (tests), or
 * omitted (defaults to http://localhost:8844).
 *
 * @param {PlacementRequest} jobSpec  the job + placement policy (payer optional; auto-filled)
 * @param {{ ce?: CeClient|string|object, now?: number, embedding?: object, scoreFn?: Function }} [opts]
 * @returns {Promise<PlacementPlan>}
 */
export async function planPlacement(jobSpec, opts = {}) {
  if (!jobSpec || typeof jobSpec !== "object") {
    throw new Error("planPlacement: jobSpec object is required");
  }
  const ce =
    opts.ce instanceof CeClient
      ? opts.ce
      : typeof opts.ce === "string"
        ? new CeClient(opts.ce)
        : opts.ce && typeof opts.ce === "object"
          ? opts.ce // duck-typed client (tests / preconfigured)
          : new CeClient();

  // Resolve the latency origin: caller-supplied payer wins; otherwise read this node's own id from
  // /status so "near me" is anchored at the node we are talking to.
  let payer = typeof jobSpec.payer === "string" && jobSpec.payer ? jobSpec.payer : undefined;
  if (!payer && typeof ce.status === "function") {
    try {
      const st = await ce.status();
      if (st && typeof st.node_id === "string" && st.node_id) payer = st.node_id;
    } catch {
      /* fall through — plan() will throw a clear error if payer stays undefined */
    }
  }
  if (!payer) {
    throw new Error(
      "planPlacement: could not resolve payer (no jobSpec.payer and /status had no node_id); " +
        "pass jobSpec.payer explicitly",
    );
  }

  const req = { ...jobSpec, payer };
  const { ce: _ignore, ...planOpts } = opts;
  void _ignore;
  return plan(req, ce, planOpts);
}

// ----------------------------------------------------------------------------
// Offline self-test for the facade (the modules each have their own __selftest).
//   node src/index.js
// ----------------------------------------------------------------------------

/**
 * Offline check of `planPlacement`: a duck-typed CE client supplies the read-substrate; the facade
 * must auto-resolve the payer from /status and return a vendor-diverse, distinct-operator plan.
 * @returns {Promise<{ok:true, targets:number}>}
 */
export async function __selftest() {
  const now = Math.floor(Date.now() / 1000);
  const tags = (asn) => ["docker", "gpu", `asn:${asn}`];
  const fakeCe = {
    status: async () => ({ node_id: "me", height: 1, balance: "0" }),
    netgraph: async () => [
      { peer: "us-a", rtt_ms: 5, samples: 10, last_seen_secs: now },
      { peer: "us-b", rtt_ms: 7, samples: 10, last_seen_secs: now },
      { peer: "eu-a", rtt_ms: 90, samples: 8, last_seen_secs: now },
      { peer: "ap-a", rtt_ms: 160, samples: 6, last_seen_secs: now },
    ],
    atlas: async () => [
      { node_id: "us-a", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: now - 5, tags: tags(64500) },
      { node_id: "us-b", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: now - 5, tags: tags(64500) },
      { node_id: "eu-a", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: now - 5, tags: tags(64600) },
      { node_id: "ap-a", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: now - 5, tags: tags(64700) },
    ],
    histories: async (ids) =>
      new Map(ids.map((id) => [id, { jobs_hosted: 5, heartbeats_hosted: 50, earned: "1", recent_earned: "1" }])),
    beacon: async () => ({ height: 999, hash: "deadbeefcafe" }),
  };
  const out = await planPlacement(
    { k: 3, cpuCores: 1, memMb: 256, requireTags: ["docker", "gpu"], redundancy: "verify", maxShare: 0.33 },
    { ce: fakeCe },
  );
  const ops = out.targets.map((t) => t.groups.operator);
  if (out.targets.length !== 3) throw new Error(`facade: expected 3 targets, got ${out.targets.length}`);
  if (new Set(ops).size !== ops.length) throw new Error("facade: replicas not on distinct operators");
  return { ok: true, targets: out.targets.length };
}

// Run the self-test when invoked directly: `node src/index.js`
if (typeof process !== "undefined" && process.argv && import.meta.url === `file://${process.argv[1]}`) {
  __selftest()
    .then((r) => console.log(`index.js __selftest: ok (${r.targets} targets, distinct operators)`))
    .catch((e) => {
      console.error("index.js __selftest FAILED:", e && e.message);
      process.exit(1);
    });
}
