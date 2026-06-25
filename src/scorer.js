/**
 * Per-candidate scalar scoring — pure functions, no I/O.
 *
 * Turns a feasibility-filtered Candidate + the resolved request into a [0,1] static score from four
 * convex-blended axes (latency, benchFit, trust, price). The contextual diversity penalty is NOT
 * here — it depends on what is already chosen and lives in vendor.js / placer.js.
 *
 * CONTRACT (frozen — see docs/placement-design.md §2 and "Module contracts"):
 *
 *   resolveWeights(req)                 -> { wL, wB, wT, wP, wD }   // OBJECTIVE_WEIGHTS + req.weights override + normalize wL..wP to 1
 *   latencyScore(candidate, req, graph) -> number                  // [0,1], §2.1: 1 - rtt/rttSoftCapMs, prefer measured
 *   benchFitScore(candidate, req)       -> { score, source, confidence }  // [0,1], §2.2 demand-weighted saturating match
 *   trustScore(candidate, req)          -> { score, benchmarkSuspect }    // [0,1], §2.3 log-saturating delivered-work + §9 cross-check
 *   priceScore(candidate, req)          -> number                  // [0,1], §2.4 BigInt math, ratio-only float
 *   staticScore(candidate, req, graph)  -> { score, parts:{latency,benchFit,trust,price}, benchFit:{source,confidence}, benchmarkSuspect }
 *
 * Money rule: priceScore parses askBaseUnits / req.priceCapBaseUnits as BigInt; the only float it
 * produces is the [0,1] ranking ratio. Never coerce a base-unit string straight to Number for math.
 *
 * benchFit axis mapping (read from candidate.profile, else atlas fallback with confidence<1):
 *   gflops        <- cpu.gflops_fp32          memBwGbps    <- cpu.mem_bw_gbps
 *   vramMb        <- max(gpus[].vram_mb)       fp16Tflops   <- sum(gpus[].fp16_tflops)
 *   tokensPerSec  <- llm.tokens_per_sec        diskReadMbps <- storage.read_mbps
 *   diskWriteMbps <- storage.write_mbps
 *
 * @module scorer
 */

import { OBJECTIVE_WEIGHTS, REQUEST_DEFAULTS, clamp01, withDefaults } from "./types.js";

/** @typedef {import("./types.js").Candidate} Candidate */
/** @typedef {import("./types.js").PlacementRequest} PlacementRequest */
/** @typedef {import("./types.js").Weights} Weights */
/** @typedef {import("./types.js").NodeProfile} NodeProfile */
/** @typedef {import("./types.js").Demand} Demand */

// ----------------------------------------------------------------------------
// Weights (§2.5)
// ----------------------------------------------------------------------------

/**
 * Resolve the blend weights for a request. Starts from the objective's defaults (§2.5), applies any
 * caller `req.weights` overrides, then normalizes the four convex axes (wL+wB+wT+wP) to sum to 1 so
 * the blend stays interpretable. `wD` is a separate penalty coefficient (applied during selection),
 * carried through verbatim and never folded into the normalization.
 *
 * @param {PlacementRequest} req
 * @returns {Weights}
 */
export function resolveWeights(req) {
  const objective = (req && req.objective) || REQUEST_DEFAULTS.objective;
  const base = OBJECTIVE_WEIGHTS[objective] || OBJECTIVE_WEIGHTS.balanced;
  const ov = (req && req.weights) || {};

  // Pull each axis: caller override wins, else objective default. Coerce non-finite/negative to the
  // default so a malformed override never poisons the blend.
  const pick = (key) => {
    const v = ov[key];
    return Number.isFinite(v) && v >= 0 ? v : base[key];
  };

  let wL = pick("wL");
  let wB = pick("wB");
  let wT = pick("wT");
  let wP = pick("wP");
  const wD = pick("wD");

  const sum = wL + wB + wT + wP;
  if (sum > 0) {
    wL /= sum;
    wB /= sum;
    wT /= sum;
    wP /= sum;
  } else {
    // Degenerate (all-zero) blend — fall back to an equal split so scoring still discriminates.
    wL = wB = wT = wP = 0.25;
  }

  return { wL, wB, wT, wP, wD };
}

// ----------------------------------------------------------------------------
// latency (§2.1)
// ----------------------------------------------------------------------------

/**
 * Latency sub-score in [0,1]: closer to the payer is better. Ground-truth measured RTT is preferred
 * over the embedding prediction; beyond `req.rttSoftCapMs` the score floors at 0 (the host is still
 * feasible, it just wins only on other axes). An unreachable host (Infinity RTT) scores 0.
 *
 * The candidate may carry a pre-resolved `rttMs` (set by the placer's feasibility pass); if present
 * and finite it is used verbatim, otherwise the graph is queried (measured first, then predicted).
 *
 * @param {Candidate} candidate
 * @param {PlacementRequest} req
 * @param {import("./graph.js").Graph} [graph]
 * @returns {number}
 */
export function latencyScore(candidate, req, graph) {
  const cap = numOr(req && req.rttSoftCapMs, REQUEST_DEFAULTS.rttSoftCapMs);
  if (!(cap > 0)) return 0;

  const rtt = resolveRtt(candidate, req, graph);
  if (!Number.isFinite(rtt)) return 0;

  return clamp01(1 - rtt / cap);
}

/**
 * Resolve the RTT (ms) from the payer to a candidate: a candidate-carried value first (placer sets
 * it), then a direct measured graph sample, then the embedding prediction. Returns Infinity if
 * nothing relates them.
 * @param {Candidate} candidate @param {PlacementRequest} req @param {import("./graph.js").Graph} [graph]
 * @returns {number}
 */
function resolveRtt(candidate, req, graph) {
  if (candidate && Number.isFinite(candidate.rttMs)) return candidate.rttMs;
  if (!graph || !req || !req.payer || !candidate) return Infinity;
  const measured = graph.measuredRtt(req.payer, candidate.nodeId);
  if (measured !== undefined && Number.isFinite(measured)) return measured;
  return graph.predictedRtt(req.payer, candidate.nodeId);
}

// ----------------------------------------------------------------------------
// benchFit (§2.2)
// ----------------------------------------------------------------------------

/**
 * The seven benchmark axes, each declaring how to read its value from a measured {@link NodeProfile}.
 * Keys match the `req.demand` axis names in types.js.
 * @type {Record<string, (p: NodeProfile) => number>}
 */
const PROFILE_AXIS = {
  gflops: (p) => num(p.cpu && p.cpu.gflops_fp32),
  memBwGbps: (p) => num(p.cpu && p.cpu.mem_bw_gbps),
  vramMb: (p) => maxOf((p.gpus || []).map((g) => num(g.vram_mb))),
  fp16Tflops: (p) => sumOf((p.gpus || []).map((g) => num(g.fp16_tflops))),
  tokensPerSec: (p) => num(p.llm && p.llm.tokens_per_sec),
  diskReadMbps: (p) => num(p.storage && p.storage.read_mbps),
  diskWriteMbps: (p) => num(p.storage && p.storage.write_mbps),
};

/**
 * benchFit: the demand-weighted, saturating match of the candidate's measured profile to the job's
 * demand vector (`req.demand`). Each per-axis fit saturates at 1.0 once the host clears the job's
 * `target` ("enough") bar — surplus hardware earns no extra fit credit (that surplus matters for
 * packing, handled by capacity, not fit).
 *
 *   fit_a       = clamp01( profile[a] / target_a )
 *   benchFit    = Σ weight_a · fit_a / Σ weight_a
 *
 * With no demand declared, benchFit is neutral (0.5) — the axis simply does not discriminate. When
 * the candidate has no signed profile, the coarse atlas signal (cores/mem/tags) is mapped to
 * estimated axes and `source = "atlas"` with `confidence < 1`; the trust term consumes that
 * confidence so unverified hardware is discounted there, not double-counted here.
 *
 * @param {Candidate} candidate
 * @param {PlacementRequest} req
 * @returns {{score:number, source:("profile"|"atlas"|"none"), confidence:number}}
 */
export function benchFitScore(candidate, req) {
  const demand = req && req.demand;
  const axes = demand ? Object.keys(demand).filter((a) => demand[a] && demand[a].weight > 0) : [];

  // No demand declared: nothing to discriminate on. Neutral fit, full confidence (we asked nothing).
  if (axes.length === 0) {
    return { score: 0.5, source: candidate && candidate.profile ? "profile" : "none", confidence: 1 };
  }

  const hasProfile = !!(candidate && candidate.profile);
  const reader = hasProfile
    ? (axis) => PROFILE_AXIS[axis](candidate.profile)
    : (axis) => atlasAxisEstimate(axis, candidate);
  const source = hasProfile ? "profile" : "atlas";
  // Measured hardware is trusted; atlas estimates are coarse → discounted confidence.
  const confidence = hasProfile ? 1 : 0.6;

  let num_ = 0;
  let den = 0;
  for (const axis of axes) {
    const { weight, target } = demand[axis];
    if (!(weight > 0) || !(target > 0)) continue;
    const value = reader(axis);
    const fit = clamp01(value / target);
    num_ += weight * fit;
    den += weight;
  }
  const score = den > 0 ? num_ / den : 0.5;
  return { score: clamp01(score), source, confidence };
}

/**
 * Coarse atlas fallback for a benchmark axis when no signed profile exists. Derives a rough estimate
 * from `cpu_cores`, `mem_mb`, and capability tags. Deliberately conservative — the discounted
 * confidence (0.6) returned alongside is what flags this as unverified.
 *
 * @param {string} axis @param {Candidate} candidate @returns {number}
 */
function atlasAxisEstimate(axis, candidate) {
  const cap = candidate && candidate.capacity;
  if (!cap) return 0;
  const cores = num(cap.cpuCores);
  const memMb = num(cap.memMb);
  const tags = Array.isArray(cap.tags) ? cap.tags.map((t) => String(t).toLowerCase()) : [];
  const hasGpu = tags.includes("gpu");

  switch (axis) {
    // ~8 GFLOPS/core fp32 is a conservative modern-core estimate.
    case "gflops":
      return cores * 8;
    // ~6 GB/s per core as a rough shared-bus estimate.
    case "memBwGbps":
      return cores * 6;
    // Without a profile we cannot know VRAM; assume a modest card iff tagged gpu, else 0.
    case "vramMb":
      return hasGpu ? 8000 : 0;
    case "fp16Tflops":
      return hasGpu ? 10 : 0;
    // LLM throughput is gpu-gated; rough token/s guess only when tagged.
    case "tokensPerSec":
      return hasGpu ? 20 : 0;
    // Disk throughput unknown from atlas; assume commodity SSD if the node hosts at all.
    case "diskReadMbps":
      return memMb > 0 ? 500 : 0;
    case "diskWriteMbps":
      return memMb > 0 ? 400 : 0;
    default:
      return 0;
  }
}

// ----------------------------------------------------------------------------
// trust (§2.3 + §9 cross-check)
// ----------------------------------------------------------------------------

/**
 * trust: a reputation weight in [0,1] derived from the candidate's on-chain `/history` facts.
 *
 *   delivered     = jobs_hosted + heartbeats_hosted          (proven hosting work)
 *   base          = log1p(delivered) / log1p(trustSaturation) (log-saturating; default sat 50)
 *   recencyBoost  = recently-active hosts weighted up to ~1.0, dormant down toward ~0.5
 *   trust         = clamp01( base · recencyBoost · profileConfidence )
 *
 * §9 benchmark cross-check: a host that *claims* a large profile (high GFLOPS / VRAM / tokens/s) but
 * has implausibly little delivered work for that claim is flagged `benchmarkSuspect = true` and its
 * trust is floored toward 0 — "card claiming throughput far above delivered work", caught at the app
 * layer purely from public facts. A host with no profile and no history is simply new (low trust, not
 * suspect).
 *
 * @param {Candidate} candidate
 * @param {PlacementRequest} req
 * @returns {{score:number, benchmarkSuspect:boolean}}
 */
export function trustScore(candidate, req) {
  const sat = numOr(req && req.trustSaturation, REQUEST_DEFAULTS.trustSaturation);
  const h = candidate && candidate.history;

  const delivered = h ? num(h.jobs_hosted) + num(h.heartbeats_hosted) : 0;
  const denom = Math.log1p(sat > 0 ? sat : REQUEST_DEFAULTS.trustSaturation);
  let base = denom > 0 ? Math.log1p(delivered) / denom : 0;

  // Recency: a node still earning recently is more trustworthy than a long-dormant one. earned is a
  // base-unit STRING — compare as BigInt, never as a float. recent>0 ⇒ full boost; some all-time
  // earnings but nothing recent ⇒ mild discount; nothing ever ⇒ neutral (base already reflects it).
  let recencyBoost = 1;
  if (h) {
    const earned = toBig(h.earned);
    const recent = toBig(h.recent_earned);
    if (earned > 0n) {
      recencyBoost = recent > 0n ? 1 : 0.75;
    }
  }

  // Profile confidence: unverified (atlas-only) hardware is discounted here so it is counted once.
  const confidence = candidate && candidate.profile ? 1 : 0.85;

  let score = clamp01(base * recencyBoost * confidence);

  // §9 cross-check: does the *claimed* profile imply far more capability than delivered work supports?
  const benchmarkSuspect = isBenchmarkSuspect(candidate, delivered);
  if (benchmarkSuspect) {
    // Floor trust hard — a suspect claim should not win on reputation. Keep a sliver only if there is
    // genuine delivered work (the claim is suspect, not the node necessarily fraudulent).
    score = Math.min(score, delivered > 0 ? 0.1 : 0);
  }

  return { score: clamp01(score), benchmarkSuspect };
}

/**
 * §9 heuristic: a high-end *claimed* profile with implausibly little delivered work is suspect. We
 * compute a coarse "claim tier" from the signed profile and require a minimum amount of delivered
 * work to corroborate it. No profile ⇒ nothing to over-claim ⇒ not suspect (just unverified, handled
 * by confidence). A node that has actually done the work is never suspect.
 *
 * @param {Candidate} candidate @param {number} delivered @returns {boolean}
 */
function isBenchmarkSuspect(candidate, delivered) {
  const p = candidate && candidate.profile;
  if (!p) return false;

  const gflops = PROFILE_AXIS.gflops(p);
  const vram = PROFILE_AXIS.vramMb(p);
  const tps = PROFILE_AXIS.tokensPerSec(p);
  const tflops = PROFILE_AXIS.fp16Tflops(p);

  // A "big claim" = a high-end GPU/LLM tier (lots of VRAM, fp16 TFLOPS, or token throughput) or an
  // unusually large CPU GFLOPS figure. Tiers chosen to flag datacenter-grade claims, not laptops.
  const bigClaim = vram >= 24000 || tflops >= 50 || tps >= 100 || gflops >= 2000;
  if (!bigClaim) return false;

  // To corroborate a big claim we want a floor of delivered hosting work. Scale the floor with the
  // size of the claim so the very largest claims need the most proof.
  const requiredDelivered = vram >= 48000 || tflops >= 150 ? 20 : 5;
  return delivered < requiredDelivered;
}

// ----------------------------------------------------------------------------
// price (§2.4) — BigInt money, ratio-only float
// ----------------------------------------------------------------------------

/**
 * price: cheaper is better, normalized against `req.priceCapBaseUnits`. All money math is BigInt over
 * base-unit strings; the ONLY float produced is the final [0,1] ranking ratio (never an amount, never
 * written back as money). A host that advertises no ask gets `req.defaultPriceScore` (neutral 0.5).
 *
 *   price = clamp01( 1 - ask / priceCap )     // ask, priceCap as BigInt base units
 *
 * @param {Candidate} candidate
 * @param {PlacementRequest} req
 * @returns {number}
 */
export function priceScore(candidate, req) {
  const ask = candidate && candidate.askBaseUnits;
  const defaultScore = numOr(req && req.defaultPriceScore, REQUEST_DEFAULTS.defaultPriceScore);

  // No advertised price → neutral.
  if (ask === undefined || ask === null || ask === "") return clamp01(defaultScore);

  const capStr = req && req.priceCapBaseUnits;
  // No cap to normalize against → cannot rank on price; neutral.
  if (capStr === undefined || capStr === null || capStr === "") return clamp01(defaultScore);

  let askBig;
  let capBig;
  try {
    askBig = BigInt(String(ask));
    capBig = BigInt(String(capStr));
  } catch {
    // Malformed money string → do not guess; neutral.
    return clamp01(defaultScore);
  }
  if (capBig <= 0n) return clamp01(defaultScore);
  if (askBig <= 0n) return 1; // free is the best possible price.
  if (askBig >= capBig) return 0; // at/over the cap contributes nothing.

  // ratio = ask / cap in [0,1). Compute with extra integer precision then convert ONCE to a float
  // ratio (this float is a ranking score, not money).
  const SCALE = 1000000n;
  const scaled = (askBig * SCALE) / capBig; // floor division, in [0, SCALE)
  const ratio = Number(scaled) / Number(SCALE);
  return clamp01(1 - ratio);
}

// ----------------------------------------------------------------------------
// static blend (§2)
// ----------------------------------------------------------------------------

/**
 * The convex blend of the four static axes (latency, benchFit, trust, price). This is the single
 * entry the placer calls per candidate. The contextual diversity penalty is intentionally NOT here —
 * it depends on what is already chosen and is applied by the placer using vendor.js.
 *
 *   score = wL·latency + wB·benchFit + wT·trust + wP·price
 *
 * @param {Candidate} candidate
 * @param {PlacementRequest} req
 * @param {import("./graph.js").Graph} [graph]
 * @returns {{score:number, parts:{latency:number,benchFit:number,trust:number,price:number}, benchFit:{source:string,confidence:number}, benchmarkSuspect:boolean}}
 */
export function staticScore(candidate, req, graph) {
  const w = resolveWeights(req);

  const latency = latencyScore(candidate, req, graph);
  const bench = benchFitScore(candidate, req);
  const trust = trustScore(candidate, req);
  const price = priceScore(candidate, req);

  const parts = {
    latency,
    benchFit: bench.score,
    trust: trust.score,
    price,
  };

  const blended = clamp01(
    w.wL * latency + w.wB * bench.score + w.wT * trust.score + w.wP * price,
  );
  // Runtime-kind adjustment: browser-WASM nodes execute jobs ~5-10x slower than native, so downrank
  // them (harder when the job is throughput-bound); and prefer the payer's own node when it is itself
  // a candidate, since running locally avoids the WAN hop entirely. Applied AFTER the convex blend so
  // the axis parts stay interpretable; surfaced in `parts.runtimeFactor` for auditability.
  const runtimeFactor = runtimeKindFactor(candidate, req, w);
  const score = clamp01(blended * runtimeFactor);
  parts.runtimeFactor = runtimeFactor;

  return {
    score,
    parts,
    benchFit: { source: bench.source, confidence: bench.confidence },
    benchmarkSuspect: trust.benchmarkSuspect,
  };
}

/** Browser-WASM throughput is ~5-10x native; base multiplier applied to a browser candidate's score. */
export const BROWSER_RUNTIME_PENALTY = 0.6;
/** Extra throughput discount: a browser is penalized more as the benchFit weight (wB) rises. */
export const BROWSER_THROUGHPUT_DISCOUNT = 0.3;
/** Boost for the payer's own node (zero network hop), when `req.localNodeId` is set and matches. */
export const PREFER_LOCAL_BOOST = 1.15;

/**
 * Multiplicative score adjustment for a candidate's runtime kind + locality. Returns 1 for a native
 * remote node with no locality signal (i.e. no change). Pure.
 *
 * @param {Candidate} candidate @param {any} req @param {{wB:number}} w resolved weights
 * @returns {number} a finite factor in (0, ~1.15]
 */
export function runtimeKindFactor(candidate, req, w) {
  let f = 1;
  const kind =
    candidate && candidate.profile && candidate.profile.runtime && candidate.profile.runtime.kind;
  if (kind === "Browser") {
    const wB = w && Number.isFinite(w.wB) ? w.wB : 0;
    f *= BROWSER_RUNTIME_PENALTY * (1 - BROWSER_THROUGHPUT_DISCOUNT * clamp01(wB));
  }
  // Prefer-local is opt-in: the caller sets req.localNodeId to its own node id. No assumption about
  // any other request field, so this never fires unless explicitly requested.
  if (req && req.localNodeId && candidate && candidate.node_id === req.localNodeId) {
    f *= PREFER_LOCAL_BOOST;
  }
  return f;
}

// ----------------------------------------------------------------------------
// small numeric / money helpers
// ----------------------------------------------------------------------------

/** Coerce to a finite number, else 0. @param {unknown} x @returns {number} */
function num(x) {
  const n = Number(x);
  return Number.isFinite(n) ? n : 0;
}

/** Finite number `x` if positive-or-zero, else fallback. @param {unknown} x @param {number} fallback */
function numOr(x, fallback) {
  const n = Number(x);
  return Number.isFinite(n) && n >= 0 ? n : fallback;
}

/** Max of an array, 0 if empty. @param {number[]} xs @returns {number} */
function maxOf(xs) {
  let m = 0;
  for (const x of xs) if (x > m) m = x;
  return m;
}

/** Sum of an array. @param {number[]} xs @returns {number} */
function sumOf(xs) {
  let s = 0;
  for (const x of xs) s += x;
  return s;
}

/**
 * Parse a base-unit money STRING to BigInt without ever going through a float. Tolerant of numbers,
 * empty, and malformed input (→ 0n) so trust scoring degrades gracefully.
 * @param {unknown} x @returns {bigint}
 */
function toBig(x) {
  if (x === undefined || x === null || x === "") return 0n;
  try {
    return BigInt(String(x));
  } catch {
    return 0n;
  }
}

// ----------------------------------------------------------------------------
// Offline self-test. Runs on synthetic candidate/profile/history fixtures, no node / no network.
//   node -e "import('./src/scorer.js').then(m => console.log(m.__selftest()))"
// or via the barrel. Throws on the first failed invariant; returns a summary on success.
// ----------------------------------------------------------------------------

/**
 * Offline correctness harness for scorer.js. Runs on synthetic candidate/history/profile fixtures
 * (no node, no network) and asserts the scoring invariants the upstream placer/vendor rely on:
 *
 *  - resolveWeights normalizes the four convex axes to 1 and carries wD through verbatim,
 *  - latencyScore prefers low-RTT (near host outscores far host) and floors past the soft cap →
 *    this is what makes the placer "prefer low-RTT",
 *  - benchFitScore saturates at the target (no extra credit for surplus hardware) and discounts
 *    confidence for atlas-only (profile-less) hosts,
 *  - trustScore log-saturates with delivered work and flags + floors a §9 benchmark-suspect host
 *    (a big GPU claim with no delivered work) → this is what makes the placer "require redundancy /
 *    avoid trusting" low-trust and suspect hosts,
 *  - priceScore is pure BigInt money (huge base-unit strings well beyond 2^53) with a ratio-only
 *    float, monotonic (cheaper scores higher),
 *  - staticScore is a convex blend in [0,1] and, on a vendor-diverse pool, ranks the near/honest
 *    host above the far/suspect one (the substrate the placer + vendor.js spread across operators).
 *
 * @returns {{ ok: true, checks: number }}
 */
export function __selftest() {
  let checks = 0;
  /** @param {boolean} cond @param {string} msg */
  const assert = (cond, msg) => {
    checks++;
    if (!cond) throw new Error(`scorer.__selftest FAILED: ${msg}`);
  };

  // --- resolveWeights ------------------------------------------------------
  {
    const w = resolveWeights({ objective: "latency" });
    const sum = w.wL + w.wB + w.wT + w.wP;
    assert(Math.abs(sum - 1) < 1e-9, `convex weights must sum to 1, got ${sum}`);
    assert(w.wL > w.wB && w.wL > w.wP, "latency objective must weight latency highest");
    assert(Math.abs(w.wD - 0.1) < 1e-9, "wD must pass through verbatim, not be normalized");

    const def = resolveWeights({});
    assert(Math.abs(def.wL + def.wB + def.wT + def.wP - 1) < 1e-9, "default (balanced) weights normalize to 1");

    // Caller override is honored and re-normalized.
    const ov = resolveWeights({ objective: "balanced", weights: { wL: 2, wB: 2, wT: 0, wP: 0 } });
    assert(Math.abs(ov.wL - 0.5) < 1e-9 && Math.abs(ov.wB - 0.5) < 1e-9, "overridden weights re-normalize");
    assert(ov.wT === 0 && ov.wP === 0, "zeroed axes stay zero after normalization");

    // Degenerate all-zero override falls back to an equal split.
    const deg = resolveWeights({ weights: { wL: 0, wB: 0, wT: 0, wP: 0 } });
    assert(deg.wL === 0.25 && deg.wP === 0.25, "all-zero blend falls back to equal split");
  }

  // --- runtimeKindFactor (browser penalty + prefer-local) ------------------
  {
    const w = resolveWeights({ objective: "throughput" });
    const native = runtimeKindFactor({ node_id: "n", profile: { runtime: { kind: "Native" } } }, {}, w);
    assert(native === 1, "native node gets no runtime adjustment");

    const browser = runtimeKindFactor({ node_id: "b", profile: { runtime: { kind: "Browser" } } }, {}, w);
    assert(browser < 1, "browser node is penalized");
    assert(browser <= BROWSER_RUNTIME_PENALTY, "browser penalized at least the base on throughput");

    // The browser penalty is harsher on throughput than on latency objectives.
    const wL = resolveWeights({ objective: "latency" });
    const browserLat = runtimeKindFactor({ node_id: "b", profile: { runtime: { kind: "Browser" } } }, {}, wL);
    assert(browserLat > browser, "browser penalized harder when throughput-bound (high wB)");

    // No profile -> unknown kind -> no penalty (treated as native/unknown, not punished).
    assert(runtimeKindFactor({ node_id: "x" }, {}, w) === 1, "missing profile -> no runtime penalty");

    // Prefer-local: opt-in via req.localNodeId; only the matching node is boosted.
    const local = runtimeKindFactor({ node_id: "me", profile: { runtime: { kind: "Native" } } }, { localNodeId: "me" }, w);
    assert(Math.abs(local - PREFER_LOCAL_BOOST) < 1e-9, "payer's own node gets the prefer-local boost");
    const remote = runtimeKindFactor({ node_id: "other", profile: { runtime: { kind: "Native" } } }, { localNodeId: "me" }, w);
    assert(remote === 1, "a different node is not boosted by prefer-local");

    // staticScore surfaces the factor and a browser candidate ranks below an identical native one.
    const prof = (kind) => ({ cpu: { gflops_fp32: 100 }, runtime: { kind } });
    const cNative = { node_id: "nn", profile: prof("Native") };
    const cBrowser = { node_id: "bb", profile: prof("Browser") };
    const req = { objective: "throughput" };
    const sN = staticScore(cNative, req, undefined);
    const sB = staticScore(cBrowser, req, undefined);
    assert(typeof sN.parts.runtimeFactor === "number", "staticScore exposes runtimeFactor part");
    assert(sN.parts.runtimeFactor === 1 && sB.parts.runtimeFactor < 1, "browser candidate carries the penalty");
    assert(sB.score < sN.score, "identical browser node ranks below the native one (slower wasm)");
  }

  // Fixture topology: payer "me" is near us-near (10 ms) and far from us-far (300 ms, past the soft
  // cap). Candidates carry an explicit rttMs so the latency tests do not hard-depend on graph
  // internals; the graph-backed branch (no candidate.rttMs) is exercised at the end with a stub graph.

  // --- latencyScore: prefers low-RTT, floors past the cap ------------------
  {
    const near = { nodeId: "us-near", rttMs: 10 };
    const far = { nodeId: "us-far", rttMs: 300 };
    const req = { rttSoftCapMs: 250 };
    const sNear = latencyScore(/** @type {any} */ (near), /** @type {any} */ (req));
    const sFar = latencyScore(/** @type {any} */ (far), /** @type {any} */ (req));
    assert(Math.abs(sNear - (1 - 10 / 250)) < 1e-9, `near latency = 1-10/250, got ${sNear}`);
    assert(sNear > sFar, "near host must outscore far host (prefers low-RTT)");
    assert(sFar === 0, "host past the soft cap floors at 0");

    // Unreachable / infinite RTT → 0.
    const unreachable = { nodeId: "x", rttMs: Infinity };
    assert(latencyScore(/** @type {any} */ (unreachable), /** @type {any} */ (req)) === 0, "unreachable host scores 0");
  }

  // --- benchFitScore: saturates at target; atlas confidence discounted -----
  {
    /** @type {any} */
    const profiled = {
      nodeId: "gpu1",
      profile: {
        node_id: "gpu1",
        cpu: { cores: 8, threads: 16, gflops_fp32: 400, mem_bw_gbps: 50 },
        gpus: [{ model: "X", backend: "Cuda", vram_mb: 24000, fp16_tflops: 80 }],
        memory: { total_mb: 16000, available_mb: 12000 },
        storage: { total_gb: 1000, free_gb: 500, read_mbps: 2000, write_mbps: 1500 },
        llm: { ref_model: "m", tokens_per_sec: 120 },
        runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: true, wasm: true },
      },
    };
    const reqVram = { demand: { vramMb: { weight: 1, target: 8000 } } };
    const fit = benchFitScore(profiled, /** @type {any} */ (reqVram));
    assert(fit.source === "profile" && fit.confidence === 1, "profiled host => source profile, confidence 1");
    assert(fit.score === 1, "24GB VRAM clears an 8GB target => saturated fit 1.0");

    // Surplus earns no extra credit: doubling the target still gives a host with 24GB → 1.0 only when
    // it clears it; below the bar it is a ratio.
    const reqBig = { demand: { vramMb: { weight: 1, target: 48000 } } };
    const fitBig = benchFitScore(profiled, /** @type {any} */ (reqBig));
    assert(Math.abs(fitBig.score - 24000 / 48000) < 1e-9, "below-bar fit is the ratio (0.5 here)");

    // No demand => neutral 0.5, full confidence.
    const neutral = benchFitScore(profiled, /** @type {any} */ ({}));
    assert(neutral.score === 0.5 && neutral.confidence === 1, "no demand => neutral fit");

    // Atlas fallback (no profile) => source atlas, confidence < 1.
    /** @type {any} */
    const atlasOnly = { nodeId: "cpu1", capacity: { cpuCores: 16, memMb: 32000, tags: ["docker"] } };
    const reqGflops = { demand: { gflops: { weight: 1, target: 64 } } };
    const fitAtlas = benchFitScore(atlasOnly, /** @type {any} */ (reqGflops));
    assert(fitAtlas.source === "atlas" && fitAtlas.confidence < 1, "no profile => atlas source, discounted confidence");
    assert(fitAtlas.score === 1, "16 cores * 8 GFLOPS = 128 clears a 64 target");

    // Demand-weighted mean across two axes.
    /** @type {any} */
    const reqMulti = {
      demand: {
        vramMb: { weight: 3, target: 48000 }, // fit 0.5
        tokensPerSec: { weight: 1, target: 60 }, // fit 1.0 (120/60 saturates)
      },
    };
    const fitMulti = benchFitScore(profiled, reqMulti);
    const expected = (3 * 0.5 + 1 * 1) / 4;
    assert(Math.abs(fitMulti.score - expected) < 1e-9, `demand-weighted mean = ${expected}, got ${fitMulti.score}`);
  }

  // --- trustScore: log-saturating + §9 benchmark-suspect floor -------------
  {
    const req = { trustSaturation: 50 };
    const newbie = { nodeId: "n", history: { jobs_hosted: 0, heartbeats_hosted: 0, earned: "0", recent_earned: "0" } };
    const veteran = {
      nodeId: "v",
      history: { jobs_hosted: 40, heartbeats_hosted: 10, earned: "7200000000000000000000", recent_earned: "1200000000000000000000" },
    };
    const tNew = trustScore(/** @type {any} */ (newbie), /** @type {any} */ (req));
    const tVet = trustScore(/** @type {any} */ (veteran), /** @type {any} */ (req));
    assert(tNew.score >= 0 && tNew.score < 0.2, `newbie trust must be low, got ${tNew.score}`);
    assert(tVet.score > tNew.score, "veteran must outrank newbie on trust");
    assert(!tNew.benchmarkSuspect && !tVet.benchmarkSuspect, "honest histories are not suspect");

    // Log saturation: 500 delivered is only modestly above 50.
    const huge = { nodeId: "h", history: { jobs_hosted: 500, heartbeats_hosted: 0, earned: "1", recent_earned: "1" } };
    const tHuge = trustScore(/** @type {any} */ (huge), /** @type {any} */ (req));
    const tFifty = trustScore(
      /** @type {any} */ ({ nodeId: "f", history: { jobs_hosted: 50, heartbeats_hosted: 0, earned: "1", recent_earned: "1" } }),
      /** @type {any} */ (req),
    );
    assert(tHuge.score - tFifty.score < tFifty.score, "trust saturates: 500 vs 50 gap < 50 vs 0 gap (log curve)");

    // Recency discount: all-time earnings but nothing recent.
    const dormant = {
      nodeId: "d",
      history: { jobs_hosted: 40, heartbeats_hosted: 10, earned: "7200000000000000000000", recent_earned: "0" },
    };
    const tDormant = trustScore(/** @type {any} */ (dormant), /** @type {any} */ (req));
    assert(tDormant.score < tVet.score, "dormant (no recent earnings) trust < active veteran");

    // §9: big GPU/LLM claim with no delivered work => suspect + trust floored.
    /** @type {any} */
    const suspect = {
      nodeId: "s",
      profile: {
        node_id: "s",
        cpu: { cores: 4, threads: 8, gflops_fp32: 100, mem_bw_gbps: 20 },
        gpus: [{ model: "H100", backend: "Cuda", vram_mb: 80000, fp16_tflops: 200 }],
        memory: { total_mb: 8000, available_mb: 6000 },
        storage: { total_gb: 100, free_gb: 50, read_mbps: 500, write_mbps: 400 },
        llm: { ref_model: "m", tokens_per_sec: 300 },
        runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: false, wasm: false },
      },
      history: { jobs_hosted: 0, heartbeats_hosted: 0, earned: "0", recent_earned: "0" },
    };
    const tSus = trustScore(suspect, /** @type {any} */ (req));
    assert(tSus.benchmarkSuspect === true, "huge claim + no delivered work => benchmarkSuspect");
    assert(tSus.score === 0, "suspect host with zero delivered work => trust floored to 0");

    // A node that has actually delivered the work for its big claim is NOT suspect.
    /** @type {any} */
    const earnedBig = { ...suspect, history: { jobs_hosted: 30, heartbeats_hosted: 0, earned: "1", recent_earned: "1" } };
    const tEarned = trustScore(earnedBig, /** @type {any} */ (req));
    assert(tEarned.benchmarkSuspect === false, "delivered work corroborates the claim => not suspect");
    assert(tEarned.score > 0, "corroborated big-claim host keeps positive trust");

    // No history at all => low trust, not suspect.
    const empty = trustScore(/** @type {any} */ ({ nodeId: "e" }), /** @type {any} */ (req));
    assert(empty.score === 0 && !empty.benchmarkSuspect, "no history => zero trust, not suspect");
  }

  // --- priceScore: BigInt money, ratio-only float, monotonic ---------------
  {
    // base units beyond 2^53 to prove no float coercion of money.
    const cap = "1000000000000000000000"; // 1000 credits
    const cheap = { nodeId: "c", askBaseUnits: "100000000000000000000" }; // 100 credits
    const dear = { nodeId: "d", askBaseUnits: "900000000000000000000" }; // 900 credits
    const req = { priceCapBaseUnits: cap, defaultPriceScore: 0.5 };
    const sCheap = priceScore(/** @type {any} */ (cheap), /** @type {any} */ (req));
    const sDear = priceScore(/** @type {any} */ (dear), /** @type {any} */ (req));
    assert(sCheap > sDear, "cheaper must score higher (monotonic)");
    assert(Math.abs(sCheap - 0.9) < 1e-6, `100/1000 => 0.9, got ${sCheap}`);
    assert(Math.abs(sDear - 0.1) < 1e-6, `900/1000 => 0.1, got ${sDear}`);

    // At/over the cap => 0; free => 1.
    assert(priceScore(/** @type {any} */ ({ askBaseUnits: cap }), /** @type {any} */ (req)) === 0, "at the cap => 0");
    assert(priceScore(/** @type {any} */ ({ askBaseUnits: "0" }), /** @type {any} */ (req)) === 1, "free => 1");

    // No ask => default neutral; no cap => default neutral.
    assert(priceScore(/** @type {any} */ ({}), /** @type {any} */ (req)) === 0.5, "no ask => default 0.5");
    assert(
      priceScore(/** @type {any} */ ({ askBaseUnits: "5" }), /** @type {any} */ ({ defaultPriceScore: 0.5 })) === 0.5,
      "no cap => default 0.5",
    );

    // Malformed money strings degrade to neutral, never throw.
    assert(
      priceScore(/** @type {any} */ ({ askBaseUnits: "not-a-number" }), /** @type {any} */ (req)) === 0.5,
      "malformed ask => neutral, no throw",
    );
  }

  // --- staticScore: convex blend ranks near/honest above far/suspect -------
  {
    const req = {
      objective: "balanced",
      payer: "me",
      rttSoftCapMs: 250,
      trustSaturation: 50,
      demand: { vramMb: { weight: 1, target: 8000 } },
    };

    /** @type {any} */
    const good = {
      nodeId: "us-near",
      rttMs: 10,
      capacity: { cpuCores: 8, memMb: 16000, tags: ["docker", "gpu"] },
      profile: {
        node_id: "us-near",
        cpu: { cores: 8, threads: 16, gflops_fp32: 400, mem_bw_gbps: 50 },
        gpus: [{ model: "X", backend: "Cuda", vram_mb: 16000, fp16_tflops: 40 }],
        memory: { total_mb: 16000, available_mb: 12000 },
        storage: { total_gb: 1000, free_gb: 500, read_mbps: 2000, write_mbps: 1500 },
        llm: { ref_model: "m", tokens_per_sec: 60 },
        runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: true, wasm: true },
      },
      history: { jobs_hosted: 40, heartbeats_hosted: 10, earned: "7200000000000000000000", recent_earned: "1200000000000000000000" },
    };

    /** @type {any} */
    const bad = {
      nodeId: "us-far",
      rttMs: 300, // past the cap => latency 0
      capacity: { cpuCores: 4, memMb: 8000, tags: ["docker", "gpu"] },
      profile: {
        node_id: "us-far",
        cpu: { cores: 4, threads: 8, gflops_fp32: 100, mem_bw_gbps: 20 },
        gpus: [{ model: "H100", backend: "Cuda", vram_mb: 80000, fp16_tflops: 200 }], // huge claim
        memory: { total_mb: 8000, available_mb: 6000 },
        storage: { total_gb: 100, free_gb: 50, read_mbps: 500, write_mbps: 400 },
        llm: { ref_model: "m", tokens_per_sec: 300 },
        runtime: { os: "linux", arch: "x86_64", docker: true, gvisor: false, wasm: false },
      },
      history: { jobs_hosted: 0, heartbeats_hosted: 0, earned: "0", recent_earned: "0" }, // no delivered work
    };

    const sGood = staticScore(good, /** @type {any} */ (req));
    const sBad = staticScore(bad, /** @type {any} */ (req));

    assert(sGood.score >= 0 && sGood.score <= 1, "staticScore in [0,1]");
    assert(sBad.score >= 0 && sBad.score <= 1, "staticScore in [0,1]");
    assert(sGood.score > sBad.score, "near/honest host must outrank far/benchmark-suspect host");
    assert(sBad.benchmarkSuspect === true, "the over-claiming far host is flagged suspect in the blend");
    assert(sGood.benchFit.source === "profile" && sGood.benchFit.confidence === 1, "blend carries benchFit provenance");
    assert(
      typeof sGood.parts.latency === "number" && typeof sGood.parts.trust === "number",
      "blend exposes per-axis parts for the explorer",
    );
    // The far host scores 0 on latency; the near host gets the full latency contribution.
    assert(sGood.parts.latency > 0 && sBad.parts.latency === 0, "latency part reflects RTT (low-RTT preferred)");
  }

  // --- graph-backed latency path (no candidate.rttMs) ----------------------
  // Done last so the assertion count is stable even if graph.js changes; uses the public graph API.
  {
    // We import graph.js lazily via a synchronous require-equivalent is unavailable in ESM; instead
    // we reconstruct the measured RTT directly through a minimal stub graph implementing the two
    // methods latencyScore needs. This keeps scorer.js's selftest self-contained (no cross-module
    // import order dependency) while still exercising the graph-backed branch.
    /** @type {any} */
    const stubGraph = {
      measuredRtt: (a, b) => (a === "me" && b === "us-near" ? 10 : undefined),
      predictedRtt: (a, b) => (a === "me" && b === "us-far" ? 280 : Infinity),
    };
    const reqG = { payer: "me", rttSoftCapMs: 250 };
    const sNear = latencyScore(/** @type {any} */ ({ nodeId: "us-near" }), /** @type {any} */ (reqG), stubGraph);
    const sFar = latencyScore(/** @type {any} */ ({ nodeId: "us-far" }), /** @type {any} */ (reqG), stubGraph);
    assert(Math.abs(sNear - (1 - 10 / 250)) < 1e-9, "graph-backed measured RTT used for near host");
    assert(sFar === 0, "graph-backed predicted RTT past the cap floors to 0");
    const sUnknown = latencyScore(/** @type {any} */ ({ nodeId: "ghost" }), /** @type {any} */ (reqG), stubGraph);
    assert(sUnknown === 0, "graph-backed unreachable host scores 0");
  }

  void OBJECTIVE_WEIGHTS;
  void withDefaults;

  return { ok: true, checks };
}
