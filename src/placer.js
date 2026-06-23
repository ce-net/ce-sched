/**
 * The selector — orchestrates the whole placement pipeline and emits a PlacementPlan.
 *
 * The only module that touches I/O (via the CeClient); its pure core (feasible / redundancyFor /
 * select / beaconSeed) is fully testable with fixture data and a fixed beacon, so the entire policy
 * is deterministic given a seed.
 *
 * CONTRACT (frozen — see docs/placement-design.md §1,§3,§5,§6,§7 and "Module contracts"):
 *
 *   plan(req, ce, options) -> Promise<PlacementPlan>
 *       gather (ce.netgraph/atlas/histories/beacon) -> buildGraph -> feasible -> select -> assemble.
 *       options: { now?:number(secs), beaconDepth?:number, embedding?:object, scoreFn?:function }
 *
 *   feasible(atlas, req, graph, now)   -> Candidate[]   // §1 hard filter; builds Candidate from RawAtlasEntry
 *   redundancyFor(candidates, req)     -> number        // §3 effectiveK = max(req.k, replication for best feasible trust)
 *   select(candidates, req, graph, seed, scoreFn) -> { targets, relaxed, shortfall, rejected } // §5–§7
 *   beaconSeed(beacon, req)            -> number        // §6 deterministic PRNG seed from beacon{height,hash} mixed with req nonce/payer
 *
 * Pipeline (plan): documented in the function body. `select` is pure given a `seed` (and an injected
 * `scoreFn`, default `scorer.staticScore`) so the whole policy is unit-testable with fixtures + a
 * fixed beacon and no live node — exactly what `__selftest()` at the bottom exercises.
 *
 * Dependency injection: the CE client is passed to `plan`, never imported as a singleton. The scoring
 * function is likewise injectable into `select`/`plan` (the default is `staticScore`); the self-test
 * injects a synthetic latency-only scorer so it runs offline even before scorer.js is implemented.
 *
 * @module placer
 */

import { withDefaults, clamp01 } from "./types.js";
import { buildGraph } from "./graph.js";
import { staticScore, resolveWeights } from "./scorer.js";
import { tagCandidates, perGroupCap, diversityPenalty, violatesCap } from "./vendor.js";

/** @typedef {import("./types.js").PlacementRequest} PlacementRequest */
/** @typedef {import("./types.js").PlacementPlan} PlacementPlan */
/** @typedef {import("./types.js").PlanTarget} PlanTarget */
/** @typedef {import("./types.js").Candidate} Candidate */
/** @typedef {import("./types.js").NodeProfile} NodeProfile */
/** @typedef {import("./types.js").RawAtlasEntry} RawAtlasEntry */
/** @typedef {import("./types.js").NodeCapacity} NodeCapacity */
/** @typedef {import("./graph.js").Graph} Graph */
/** @typedef {import("./ce.js").CeClient} CeClient */

/**
 * A scoring function: maps a candidate + request + graph to a static score breakdown. The placer
 * calls it once per candidate. The default is `scorer.staticScore`; tests inject a synthetic one.
 * @typedef {(c: Candidate, req: PlacementRequest, graph: Graph) => {
 *   score: number,
 *   parts?: { latency:number, benchFit:number, trust:number, price:number },
 *   benchFit?: { source:string, confidence:number },
 *   benchmarkSuspect?: boolean
 * }} ScoreFn
 */

// ----------------------------------------------------------------------------
// Beacon-seeded PRNG (§6).
// ----------------------------------------------------------------------------

/**
 * mulberry32 — tiny deterministic PRNG. Same generator graph.js uses; reproduced here so the placer
 * has no cross-module coupling for its seeded tie-break / softmax.
 * @param {number} seed 32-bit
 * @returns {() => number} stream of floats in [0,1)
 */
function mulberry32(seed) {
  let s = seed >>> 0;
  return () => {
    s |= 0;
    s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Fold a string into a 32-bit hash (FNV-1a). Used to mix the beacon hash + request identity into the
 * PRNG seed so it is unpredictable before dispatch yet replayable after.
 * @param {string} str
 * @returns {number} unsigned 32-bit
 */
function hash32(str) {
  let h = 0x811c9dc5;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/**
 * §6 deterministic PRNG seed from the beacon mixed with the request identity (nonce/payer) so the
 * seed is unpredictable before dispatch yet auditable after: anyone can replay
 * `(beacon, request, candidate set) -> same plan`. Pure.
 * @param {{ height:number, hash:string }} beacon
 * @param {PlacementRequest} req
 * @returns {number} 32-bit seed
 */
export function beaconSeed(beacon, req) {
  const height = beacon && Number.isFinite(beacon.height) ? beacon.height >>> 0 : 0;
  const beaconHash = beacon && typeof beacon.hash === "string" ? beacon.hash : "";
  const nonce = req && typeof req.nonce === "string" ? req.nonce : "";
  const payer = req && typeof req.payer === "string" ? req.payer : "";
  // Mix all four identity components. XOR-fold so the order of contributions is irrelevant and every
  // component perturbs the whole 32-bit space.
  let seed = height;
  seed = (seed ^ hash32(beaconHash)) >>> 0;
  seed = (seed ^ Math.imul(hash32(payer), 0x9e3779b1)) >>> 0;
  seed = (seed ^ Math.imul(hash32(nonce), 0x85ebca6b)) >>> 0;
  // Guarantee a non-zero seed (mulberry32 with 0 still streams, but keep it deterministic + lively).
  return seed === 0 ? 0x6d2b79f5 : seed >>> 0;
}

// ----------------------------------------------------------------------------
// §1 — candidate build (hard feasibility filter).
// ----------------------------------------------------------------------------

/**
 * Normalize a raw atlas row (snake_case) into a NodeCapacity (camelCase). Mirrors graph.js's private
 * `toCapacity` so feasible() can build a Candidate directly from the atlas it filters.
 * @param {RawAtlasEntry} e
 * @returns {NodeCapacity}
 */
function capacityOf(e) {
  return {
    nodeId: e.node_id,
    cpuCores: Number(e.cpu_cores ?? 0),
    memMb: Number(e.mem_mb ?? 0),
    runningJobs: Number(e.running_jobs ?? 0),
    lastSeenSecs: Number(e.last_seen_secs ?? 0),
    tags: Array.isArray(e.tags) ? e.tags.slice() : [],
  };
}

/**
 * Estimated CPU cores already committed on a host. With no per-job accounting on the wire we charge a
 * conservative one core per running job (the same coarse heuristic the design's §1.2 fallback uses).
 * @param {NodeCapacity} cap
 * @returns {number}
 */
function estimatedUsedCores(cap) {
  return Math.max(0, Number(cap.runningJobs) || 0);
}

/**
 * Available memory (MB) for a candidate. With a NodeProfile, `memory.available_mb` is authoritative;
 * without one, fall back to advertised `mem_mb` minus a per-running-job reservation.
 * @param {NodeCapacity} cap
 * @param {NodeProfile|null} profile
 * @returns {number}
 */
function availableMemMb(cap, profile) {
  if (profile && profile.memory && Number.isFinite(profile.memory.available_mb)) {
    return Number(profile.memory.available_mb);
  }
  // Coarse fallback: assume each running job holds ~512 MB. Never negative.
  const reserved = (Number(cap.runningJobs) || 0) * 512;
  return Math.max(0, (Number(cap.memMb) || 0) - reserved);
}

/**
 * Read a profile axis (VRAM / GFLOPS / tokens-per-sec) for the §1.4 capability floor. Returns
 * undefined when the axis is not measured (no profile or missing field).
 * @param {NodeProfile|null} profile
 * @param {"gflops"|"vram"|"tokens"} axis
 * @returns {number|undefined}
 */
function profileAxis(profile, axis) {
  if (!profile) return undefined;
  if (axis === "gflops") return profile.cpu ? Number(profile.cpu.gflops_fp32) : undefined;
  if (axis === "tokens") return profile.llm ? Number(profile.llm.tokens_per_sec) : undefined;
  if (axis === "vram") {
    if (!Array.isArray(profile.gpus) || profile.gpus.length === 0) return undefined;
    return profile.gpus.reduce((m, g) => Math.max(m, Number(g.vram_mb) || 0), 0);
  }
  return undefined;
}

/**
 * §1 hard feasibility filter: RawAtlasEntry[] -> Candidate[]. Pure. Each survivor is a fully-formed
 * Candidate (capacity, profile-or-null, history:null until plan() attaches it, measured/predicted
 * RTT to the payer, advertised ask if any). Hard constraints are pass/fail and never trade off
 * against score:
 *   1. liveness          now - lastSeenSecs <= maxStaleSecs
 *   2. resource headroom  free cores >= cpuCores AND available mem >= memMb
 *   3. required tags      every req.requireTags present
 *   4. capability floor   minGflops/minVramMb/minTokensPerSec met by a measured profile;
 *                         no profile + a floor => excluded iff req.requireProfile
 *   5. reachability       finite predictedRtt(payer, candidate)
 *   6. self / exclusion   not in req.exclude, and (unless allowSelf) not the payer itself
 * @param {RawAtlasEntry[]} atlas
 * @param {PlacementRequest} req  (already withDefaults-resolved by plan(); also self-resolved here)
 * @param {Graph} graph
 * @param {number} now unix secs
 * @returns {Candidate[]}
 */
export function feasible(atlas, req, graph, now) {
  const r = withDefaults(req);
  const rows = Array.isArray(atlas) ? atlas : [];
  const exclude = new Set(Array.isArray(r.exclude) ? r.exclude : []);
  const requireTags = Array.isArray(r.requireTags) ? r.requireTags : [];
  const hasFloor =
    Number.isFinite(r.minGflops) || Number.isFinite(r.minVramMb) || Number.isFinite(r.minTokensPerSec);

  /** @type {Candidate[]} */
  const out = [];
  for (const e of rows) {
    if (!e || typeof e.node_id !== "string") continue;
    const nodeId = e.node_id;

    // 6. self / exclusion (cheapest, do first).
    if (exclude.has(nodeId)) continue;
    if (!r.allowSelf && nodeId === r.payer) continue;

    const cap = capacityOf(e);
    const profile = e.profile ?? null;

    // 1. liveness.
    const age = now - cap.lastSeenSecs;
    if (!Number.isFinite(age) || age > r.maxStaleSecs) continue;

    // 2. resource headroom.
    const freeCores = cap.cpuCores - estimatedUsedCores(cap);
    if (freeCores < r.cpuCores) continue;
    if (availableMemMb(cap, profile) < r.memMb) continue;

    // 3. required tags.
    let tagsOk = true;
    for (const t of requireTags) {
      if (!cap.tags.includes(t)) {
        tagsOk = false;
        break;
      }
    }
    if (!tagsOk) continue;

    // 4. capability floor. If a floor is declared and there is no measured profile, admit only when
    //    requireProfile is false (unverified hardware — discounted later by trust, not excluded here).
    if (hasFloor) {
      if (!profile) {
        if (r.requireProfile) continue;
      } else {
        const gf = profileAxis(profile, "gflops");
        const vr = profileAxis(profile, "vram");
        const tk = profileAxis(profile, "tokens");
        if (Number.isFinite(r.minGflops) && (gf === undefined || gf < r.minGflops)) continue;
        if (Number.isFinite(r.minVramMb) && (vr === undefined || vr < r.minVramMb)) continue;
        if (Number.isFinite(r.minTokensPerSec) && (tk === undefined || tk < r.minTokensPerSec)) continue;
      }
    }

    // 5. reachability — an unreachable host cannot serve. measuredRtt is ground truth when present.
    const measured = graph ? graph.measuredRtt(r.payer, nodeId) : undefined;
    const rttMeasured = measured !== undefined;
    const rttMs = rttMeasured ? measured : graph ? graph.predictedRtt(r.payer, nodeId) : Infinity;
    if (!Number.isFinite(rttMs)) continue;

    out.push({
      nodeId,
      capacity: cap,
      profile,
      history: null,
      rttMs,
      rttMeasured,
      askBaseUnits: typeof e.ask_base_units === "string" ? e.ask_base_units : undefined,
    });
  }
  return out;
}

// ----------------------------------------------------------------------------
// §3 — redundancy factor (how many hosts).
// ----------------------------------------------------------------------------

/**
 * Coarse trust estimate of a candidate from its /history facts, in [0,1], used ONLY to decide the
 * replication count (the real, weighted trust score is the scorer's job during selection). Reads the
 * delivered-work count (jobs + heartbeats hosted) on a log-saturating curve. No profile/history => 0.
 * @param {Candidate} c
 * @param {PlacementRequest} req
 * @returns {number}
 */
function coarseTrust(c, req) {
  const h = c && c.history;
  if (!h || typeof h !== "object") return c && c.profile ? 0.05 : 0; // a profile alone is weak signal
  const jobs = Number(/** @type {any} */ (h).jobs_hosted ?? 0) || 0;
  const heartbeats = Number(/** @type {any} */ (h).heartbeats_hosted ?? 0) || 0;
  const delivered = jobs + heartbeats;
  const sat = Number.isFinite(req.trustSaturation) && req.trustSaturation > 0 ? req.trustSaturation : 50;
  if (delivered <= 0) return 0;
  return clamp01(Math.log1p(delivered) / Math.log1p(sat));
}

/**
 * §3 effectiveK: max(req.k, replication implied by the best feasible trust + req.redundancy). Pure.
 *
 *   redundancy = "none"            -> req.k (trust the single best).
 *   redundancy = "verify"          -> at least 3 INDEPENDENT replicas; if the best reachable host is
 *                                     already high-trust, 2 suffice (still cross-checked), but never < 2.
 *   redundancy = <number> (target  -> a target confidence in (0,1): map the best feasible trust to the
 *   confidence)                       replica count needed to reach it. High-trust hosts => fewer.
 * The replication count is bounded by the candidate pool size (cannot place more than exist).
 * @param {Candidate[]} candidates
 * @param {PlacementRequest} req
 * @returns {number}
 */
export function redundancyFor(candidates, req) {
  const r = withDefaults(req);
  const baseK = Number.isFinite(r.k) && r.k > 0 ? Math.floor(r.k) : 1;
  const list = Array.isArray(candidates) ? candidates : [];
  const poolSize = list.length;

  // Best feasible trust drives how much redundancy a policy actually demands.
  let bestTrust = 0;
  for (const c of list) bestTrust = Math.max(bestTrust, coarseTrust(c, r));

  let replication = baseK;
  const policy = r.redundancy;
  if (policy === "verify") {
    // Verification needs independent replicas to majority-vote. High-trust host => 2 (still compared);
    // anything less => 3.
    replication = bestTrust >= 0.6 ? 2 : 3;
  } else if (typeof policy === "number" && Number.isFinite(policy) && policy > 0 && policy < 1) {
    // Target confidence. Per-host success prob ~ 0.5 + 0.49*bestTrust (an unverified host is a coin
    // flip; a fully-trusted host nearly always delivers). Replicas needed so 1-(1-p)^n >= target.
    const p = Math.min(0.99, 0.5 + 0.49 * bestTrust);
    const target = policy;
    let n = 1;
    while (1 - Math.pow(1 - p, n) < target && n < 16) n++;
    replication = n;
  }
  // "none" leaves replication = baseK.

  const effective = Math.max(baseK, replication);
  // Never demand more replicas than the pool can supply (a SHORT plan is reported via shortfall by
  // select(), but effectiveK itself should not exceed what is even theoretically placeable).
  return poolSize > 0 ? Math.min(effective, Math.max(baseK, poolSize)) : effective;
}

// ----------------------------------------------------------------------------
// §5–§7 — constraint-satisfaction selection.
// ----------------------------------------------------------------------------

/**
 * The default scorer the placer uses when no `scoreFn` is injected. Wraps scorer.staticScore so the
 * single integration point is here (and tests can swap it without importing scorer).
 * @type {ScoreFn}
 */
const DEFAULT_SCORE_FN = (c, req, graph) => staticScore(c, req, graph);

/**
 * Cohort co-location adjustment (§7) to a candidate's live score given what is already chosen:
 *   - "colocate": reward low predicted RTT to the chosen members (chatty / tensor-parallel jobs) —
 *     a bonus in [0, colocBonusMax] that shrinks with mean inter-member RTT.
 *   - "spread":   reward landing in a NOT-yet-used region (availability under a region-wide outage).
 *   - "dag":      documented extension; v0 treats it as "spread" (the DAG adds per-edge inter-stage
 *     RTT to the live score in a later revision; the selector core is unchanged).
 * Returns a signed delta added to the live score. Pure.
 * @param {Candidate} c
 * @param {Candidate[]} chosen
 * @param {PlacementRequest} req
 * @param {Graph} graph
 * @returns {number}
 */
function cohortAdjust(c, chosen, req, graph) {
  if (!chosen || chosen.length === 0) return 0;
  const mode = req.cohort ?? "spread";
  if (mode === "colocate") {
    if (!graph || typeof graph.predictedRtt !== "function") return 0;
    let sum = 0;
    let n = 0;
    for (const ch of chosen) {
      const rtt = graph.predictedRtt(c.nodeId, ch.nodeId);
      if (Number.isFinite(rtt)) {
        sum += rtt;
        n++;
      }
    }
    if (n === 0) return 0;
    const meanRtt = sum / n;
    const cap = Number.isFinite(req.rttSoftCapMs) && req.rttSoftCapMs > 0 ? req.rttSoftCapMs : 250;
    const colocBonusMax = 0.15;
    // Closer to the cohort => larger bonus; flattens to 0 past the soft cap.
    return colocBonusMax * clamp01(1 - meanRtt / cap);
  }
  // "spread" / "dag": small bonus for a fresh region; the diversity penalty already does the heavy work.
  const cReg = c.groups ? c.groups.region : undefined;
  if (!cReg || cReg.startsWith("r?:")) return 0; // region-less node: no spread signal
  for (const ch of chosen) {
    if (ch.groups && ch.groups.region === cReg) return 0; // region already used → no bonus
  }
  return 0.05; // fresh region bonus
}

/**
 * Softmax-sample an index from `live` scores using a beacon-seeded PRNG (§6 stochastic selection).
 * @param {{ live:number }[]} pool
 * @param {number} temperature
 * @param {() => number} rand PRNG in [0,1)
 * @returns {number} index into `pool`
 */
function softmaxSample(pool, temperature, rand) {
  const t = Number.isFinite(temperature) && temperature > 0 ? temperature : 0.15;
  let max = -Infinity;
  for (const p of pool) if (p.live > max) max = p.live;
  const weights = pool.map((p) => Math.exp((p.live - max) / t));
  const total = weights.reduce((a, b) => a + b, 0);
  if (!(total > 0)) return 0;
  let x = rand() * total;
  for (let i = 0; i < weights.length; i++) {
    x -= weights[i];
    if (x <= 0) return i;
  }
  return weights.length - 1;
}

/**
 * §5–§7 constraint-satisfaction selection. Pure given `seed` (beacon-derived) and `scoreFn`. Greedy
 * with live re-ranking, hard group/operator caps, graded relaxation, beacon tie-break or softmax, and
 * the cohort pass. Returns the chosen targets plus full provenance for the plan.
 * @param {Candidate[]} candidates  feasibility-filtered + vendor-tagged (carry `.groups`)
 * @param {PlacementRequest} req
 * @param {Graph} graph
 * @param {number} seed beacon-derived 32-bit PRNG seed
 * @param {ScoreFn} [scoreFn=DEFAULT_SCORE_FN]  injectable; default scorer.staticScore
 * @returns {{ targets: PlanTarget[], relaxed: string[], shortfall: number, rejected: {nodeId:string,reason:string}[] }}
 */
export function select(candidates, req, graph, seed, scoreFn = DEFAULT_SCORE_FN) {
  const r = withDefaults(req);
  const pool = Array.isArray(candidates) ? candidates.slice() : [];
  const rand = mulberry32(seed >>> 0);

  // Resolve blend weights once (for wD and the cohort/score math). resolveWeights may itself read
  // r.weights / r.objective; it is pure.
  const weights = safeWeights(r);
  const wD = Number.isFinite(weights.wD) ? weights.wD : 0.15;

  const effectiveK = redundancyFor(pool, r);
  const requireDistinctOperator = r.redundancy === "verify";

  // Pre-score every candidate once (static, contextual penalty applied live).
  /** @type {Map<string, {c:Candidate, static:number, parts:any, benchFit:any, benchmarkSuspect:boolean}>} */
  const scored = new Map();
  for (const c of pool) {
    let s;
    try {
      s = scoreFn(c, r, graph);
    } catch {
      s = { score: 0 };
    }
    scored.set(c.nodeId, {
      c,
      static: Number.isFinite(s.score) ? s.score : 0,
      parts: s.parts ?? null,
      benchFit: s.benchFit ?? null,
      benchmarkSuspect: !!s.benchmarkSuspect,
    });
    // Stash the resolved score onto the candidate so the explorer can show the breakdown.
    c.score = {
      total: scored.get(c.nodeId).static,
      parts: s.parts ?? null,
      benchFit: s.benchFit ?? null,
      benchmarkSuspect: !!s.benchmarkSuspect,
    };
  }

  /** @type {Candidate[]} */
  const chosen = [];
  /** @type {Set<string>} */
  const chosenIds = new Set();
  /** @type {string[]} */
  const relaxed = [];
  /** @type {Map<string,string>} */
  const rejectedReason = new Map();

  const baseCap = perGroupCap(effectiveK, r.maxShare);

  // Relaxation ladder: each entry loosens ONE soft cap. Operator stays hard under "verify"; region
  // stays hard under "colocate" never relaxes operator. We relax region -> asn -> operator, recording
  // each compromise. Index -1 = no relaxation.
  const ladder = ["region", "asn", "operator"];

  while (chosen.length < effectiveK && pool.length > chosen.length) {
    let picked = null;
    let pickedLive = -Infinity;
    let relaxStep = -1;

    // Try with progressively relaxed caps until something is admissible.
    for (let step = -1; step < ladder.length; step++) {
      const relaxedSet = new Set(ladder.slice(0, step + 1)); // step=-1 => empty
      // Under verify, the operator cap is NEVER relaxed (cross-operator independence is the point).
      if (requireDistinctOperator) relaxedSet.delete("operator");
      // Under colocate, the region cap is relaxed by design (co-located replicas share a region).
      if (r.cohort === "colocate") relaxedSet.add("region");

      /** @type {{c:Candidate, live:number}[]} */
      const live = [];
      for (const c of pool) {
        if (chosenIds.has(c.nodeId)) continue;
        const offending = capGate(c, chosen, baseCap, requireDistinctOperator, relaxedSet);
        if (offending) {
          // Only record the FIRST (un-relaxed) reason so rejected[] reflects the real cause.
          if (step === -1 && !rejectedReason.has(c.nodeId)) {
            rejectedReason.set(c.nodeId, offending === "operator" ? "operator_dup" : "group_cap");
          }
          continue;
        }
        const sc = scored.get(c.nodeId);
        const penalty = wD * diversityPenalty(c, chosen, r);
        const cohort = cohortAdjust(c, chosen, r, graph);
        live.push({ c, live: sc.static - penalty + cohort });
      }

      if (live.length === 0) continue; // nothing admissible at this relaxation level; loosen further

      live.sort((a, b) => b.live - a.live || (a.c.nodeId < b.c.nodeId ? -1 : 1));

      if (r.selection === "weighted") {
        const idx = softmaxSample(live, r.temperature, rand);
        picked = live[idx].c;
        pickedLive = live[idx].live;
      } else {
        // best + beacon tie-break among near-ties within tieEps.
        const top = live[0].live;
        const eps = Number.isFinite(r.tieEps) ? r.tieEps : 0.02;
        const ties = live.filter((x) => top - x.live <= eps);
        const winner = ties.length > 1 ? ties[Math.floor(rand() * ties.length)] : live[0];
        picked = winner.c;
        pickedLive = winner.live;
      }
      relaxStep = step;
      break;
    }

    if (!picked) break; // even fully-relaxed nothing fits — short plan.

    // Record any relaxation actually used for this pick.
    if (relaxStep >= 0) {
      for (const key of ladder.slice(0, relaxStep + 1)) {
        if (key === "operator" && requireDistinctOperator) continue;
        if (!relaxed.includes(key)) relaxed.push(key);
      }
    }

    picked.replica = chosen.length;
    chosen.push(picked);
    chosenIds.add(picked.nodeId);
    rejectedReason.delete(picked.nodeId); // it was chosen, not rejected
  }

  const shortfall = Math.max(0, effectiveK - chosen.length);

  // Anything feasible-but-unchosen with no recorded cap reason lost on score.
  /** @type {{nodeId:string, reason:string}[]} */
  const rejected = [];
  for (const c of pool) {
    if (chosenIds.has(c.nodeId)) continue;
    let reason = rejectedReason.get(c.nodeId);
    if (!reason) {
      const sc = scored.get(c.nodeId);
      reason = sc && sc.benchmarkSuspect ? "benchmark_suspect" : "low_score";
    }
    rejected.push({ nodeId: c.nodeId, reason });
  }

  /** @type {PlanTarget[]} */
  const targets = chosen.map((c) => ({
    nodeId: c.nodeId,
    score: c.score && Number.isFinite(c.score.total) ? c.score.total : 0,
    rttMs: c.rttMs,
    benchFit:
      c.score && c.score.parts && Number.isFinite(c.score.parts.benchFit)
        ? c.score.parts.benchFit
        : c.score && c.score.benchFit && Number.isFinite(c.score.benchFit.confidence)
          ? c.score.benchFit.confidence
          : 0,
    trust: c.score && c.score.parts && Number.isFinite(c.score.parts.trust) ? c.score.parts.trust : 0,
    groups: c.groups ?? { operator: c.nodeId, asn: "unknown", region: "r?", cluster: c.nodeId },
    replica: Number.isFinite(c.replica) ? c.replica : 0,
  }));

  return { targets, relaxed, shortfall, rejected };
}

/**
 * Apply the hard cap gate honoring a relaxed-group set. A group key in `relaxedSet` is not enforced
 * (its cap is loosened). The distinct-operator gate is always enforced when `requireDistinctOperator`.
 * Returns the offending group key or null. Bridges to vendor.violatesCap, which takes a single
 * `perGroup` cap; we emulate per-group relaxation by setting an effectively-infinite cap for relaxed
 * groups via two calls (count gate vs operator gate) — simpler to inline the check here.
 * @param {Candidate} c
 * @param {Candidate[]} chosen
 * @param {number} perGroup
 * @param {boolean} requireDistinctOperator
 * @param {Set<string>} relaxedSet
 * @returns {string|null}
 */
function capGate(c, chosen, perGroup, requireDistinctOperator, relaxedSet) {
  // First, the non-relaxable distinct-operator rule (verify) + the operator count cap (unless relaxed).
  const opOffending = violatesCap(c, chosen, {
    perGroup: relaxedSet.has("operator") && !requireDistinctOperator ? Infinity : perGroup,
    requireDistinctOperator,
  });
  // violatesCap returns the FIRST offending key in operator->asn->region->cluster order. We need to
  // respect the relaxed set per-key, so re-derive precisely rather than trusting one combined call.
  if (opOffending === "operator") {
    // operator violation always stands (it is either the distinct-op rule or the un-relaxed op cap).
    if (requireDistinctOperator || !relaxedSet.has("operator")) return "operator";
  }
  // Recompute each non-operator key under its own relaxation flag by calling violatesCap with that
  // key effectively uncapped when relaxed.
  const offending = violatesCap(c, chosen, {
    perGroup,
    requireDistinctOperator,
  });
  if (!offending) return null;
  if (relaxedSet.has(offending)) {
    // This specific group is relaxed; check whether a STILL-enforced group also offends. Re-run with
    // the relaxed group's members tolerated is non-trivial via the single-cap API, so fall back to a
    // manual scan over the enforced keys.
    return enforcedOffender(c, chosen, perGroup, requireDistinctOperator, relaxedSet);
  }
  return offending;
}

/**
 * Manual per-key cap check over only the ENFORCED (non-relaxed) groups. Mirrors vendor.groupCounts
 * semantics (operator always counts; asn/region only on real signals) without importing the private.
 * @param {Candidate} c @param {Candidate[]} chosen @param {number} perGroup
 * @param {boolean} requireDistinctOperator @param {Set<string>} relaxedSet
 * @returns {string|null}
 */
function enforcedOffender(c, chosen, perGroup, requireDistinctOperator, relaxedSet) {
  const cg = c.groups;
  if (!cg) return null;
  let opCount = 0;
  let asnCount = 0;
  let regionCount = 0;
  let clusterCount = 0;
  for (const ch of chosen) {
    const g = ch.groups;
    if (!g) continue;
    if (g.operator === cg.operator) opCount++;
    if (cg.asn !== "unknown" && g.asn === cg.asn) asnCount++;
    if (!cg.region.startsWith("r?:") && g.region === cg.region) regionCount++;
    if (g.cluster === cg.cluster) clusterCount++;
  }
  if (requireDistinctOperator && opCount > 0) return "operator";
  if (!relaxedSet.has("operator") && opCount + 1 > perGroup) return "operator";
  if (!relaxedSet.has("asn") && cg.asn !== "unknown" && asnCount + 1 > perGroup) return "asn";
  if (!relaxedSet.has("region") && !cg.region.startsWith("r?:") && regionCount + 1 > perGroup) return "region";
  if (!relaxedSet.has("cluster") && clusterCount + 1 > perGroup) return "cluster";
  return null;
}

/**
 * Resolve weights via scorer.resolveWeights, with a defensive fallback if scorer.js is not yet
 * implemented (the placer must remain usable / self-testable without the scorer). The fallback
 * mirrors the OBJECTIVE_WEIGHTS table for "balanced".
 * @param {PlacementRequest} req
 * @returns {{wL:number,wB:number,wT:number,wP:number,wD:number}}
 */
function safeWeights(req) {
  try {
    const w = resolveWeights(req);
    if (w && Number.isFinite(w.wD)) return w;
  } catch {
    // scorer.js stubbed — fall through to defaults.
  }
  if (req && req.weights && Number.isFinite(req.weights.wD)) return req.weights;
  return { wL: 0.25, wB: 0.25, wT: 0.25, wP: 0.1, wD: 0.15 };
}

// ----------------------------------------------------------------------------
// plan() — the high-level orchestrator (the only I/O in the module).
// ----------------------------------------------------------------------------

/**
 * High-level placement: fetch the substrate, build the graph, select targets, return the plan.
 *
 * Pipeline:
 *   1. [ng, atlas, beacon] = await Promise.all([ce.netgraph(), ce.atlas(), ce.beacon()])
 *   2. graph = buildGraph(new Map([[payer, ng]]), atlas, { embedding })   // origin = payer
 *   3. cand0 = feasible(atlas, req, graph, now)                           // §1
 *   4. hist  = await ce.histories(cand0 ids); attach to each candidate    // reputation substrate
 *   5. cand  = tagCandidates(cand0, graph)                                // vendor groups
 *   6. seed  = beaconSeed(beacon, req)                                    // §6
 *   7. { targets, relaxed, shortfall, rejected } = select(cand, req, graph, seed, scoreFn)
 *   8. assemble PlacementPlan
 *
 * @param {PlacementRequest} req
 * @param {CeClient} ce
 * @param {{ now?: number, beaconDepth?: number, embedding?: object, scoreFn?: ScoreFn }} [options]
 * @returns {Promise<PlacementPlan>}
 */
export async function plan(req, ce, options = {}) {
  if (!req || typeof req.payer !== "string" || !req.payer) {
    throw new Error("plan: req.payer (the latency origin node id) is required");
  }
  if (!ce || typeof ce.netgraph !== "function") {
    throw new Error("plan: a CeClient (or compatible) must be injected as the second argument");
  }
  const r = withDefaults(req);
  const now = Number.isFinite(options.now) ? options.now : Math.floor(Date.now() / 1000);
  const scoreFn = options.scoreFn ?? DEFAULT_SCORE_FN;

  // 1. gather the read-substrate concurrently.
  const [ng, atlas, beacon] = await Promise.all([ce.netgraph(), ce.atlas(), ce.beacon()]);

  // 2. build the latency graph anchored at the payer's vantage.
  const graph = buildGraph(new Map([[r.payer, ng]]), atlas, {
    embedding: options.embedding,
  });

  // 3. §1 hard feasibility filter.
  const cand0 = feasible(atlas, r, graph, now);

  // 4. attach /history reputation facts (concurrent, fault-tolerant).
  if (typeof ce.histories === "function" && cand0.length > 0) {
    const hist = await ce.histories(cand0.map((c) => c.nodeId));
    for (const c of cand0) c.history = hist.get(c.nodeId) ?? null;
  }

  // 5. vendor grouping keys (operator/asn/region/cluster).
  const cand = tagCandidates(cand0, graph);

  // 6. beacon-seeded PRNG seed.
  const seed = beaconSeed(beacon, r);

  // 7. constraint-satisfaction selection.
  const { targets, relaxed, shortfall, rejected } = select(cand, r, graph, seed, scoreFn);

  // 8. assemble the auditable plan.
  return {
    targets,
    effectiveK: redundancyFor(cand, r),
    requestedK: Number.isFinite(r.k) && r.k > 0 ? Math.floor(r.k) : 1,
    beacon: { height: beacon ? beacon.height : 0, hash: beacon ? beacon.hash : "" },
    weights: safeWeights(r),
    objective: r.objective,
    relaxed,
    shortfall,
    rejected,
    assembledAtMs: Date.now(),
  };
}

// ----------------------------------------------------------------------------
// Offline self-test (synthetic fixtures — no live node, no I/O, no scorer.js).
//   node -e "import('./src/placer.js').then(m => console.log(m.__selftest()))"
// ----------------------------------------------------------------------------

/**
 * Deterministic offline verification of the placer's pure core on synthetic netgraph/atlas/history
 * fixtures. Runs WITHOUT scorer.js (which is still a stub) by injecting a synthetic latency+trust
 * scoreFn, and WITHOUT a live node by using a stub graph (only the methods feasible/select/cohort
 * touch: measuredRtt, predictedRtt, regionOf, capacityOf). Asserts the behaviours the task calls out:
 *
 *   - feasible() honors liveness / headroom / tags / floor / self-exclusion,
 *   - the placer SPREADS across vendors (distinct operators) and never exceeds the per-operator cap,
 *   - it PREFERS the low-RTT host,
 *   - redundancy="verify" forces DISTINCT operators per replica AND raises effectiveK,
 *   - a too-small / over-constrained pool yields a SHORT plan (shortfall > 0) with reasons,
 *   - the beacon seed is deterministic and the tie-break is replayable,
 *   - feasible() builds full Candidate rows (rttMeasured ground-truth preference).
 *
 * @returns {{ ok: true, checks: number }}
 */
export function __selftest() {
  let checks = 0;
  /** @param {boolean} cond @param {string} msg */
  const assert = (cond, msg) => {
    checks++;
    if (!cond) throw new Error(`placer.__selftest FAILED: ${msg}`);
  };

  const NOW = 10_000;

  // --- Stub graph: 4 operators across 2 regions (us-a/us-b co-region, eu-a, lone region-less). ----
  // payer is "me"; measured RTTs to each host: us-a 5, us-b 7, eu-a 90; lone has only a predicted RTT.
  const regionMap = new Map([
    ["us-a", 0],
    ["us-b", 0],
    ["eu-a", 1],
  ]);
  const measured = new Map([
    ["me|us-a", 5],
    ["me|us-b", 7],
    ["me|eu-a", 90],
  ]);
  const predicted = new Map([
    ["me|us-a", 5],
    ["me|us-b", 7],
    ["me|eu-a", 90],
    ["me|lone", 40], // reachable only via the embedding (no direct sample)
  ]);
  const capMap = new Map([
    ["us-a", { nodeId: "us-a", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: NOW - 10, tags: ["docker", "asn:64500"] }],
    ["us-b", { nodeId: "us-b", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: NOW - 10, tags: ["docker", "asn:64500"] }],
    ["eu-a", { nodeId: "eu-a", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: NOW - 10, tags: ["docker", "asn:64600"] }],
    ["lone", { nodeId: "lone", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: NOW - 10, tags: ["docker"] }],
  ]);
  const pairKey = (a, b) => `${a}|${b}`;
  /** @type {any} */
  const graph = {
    regionOf: (n) => (regionMap.has(n) ? regionMap.get(n) : -1),
    capacityOf: (n) => capMap.get(n),
    measuredRtt: (a, b) => {
      if (a === b) return 0;
      const v = measured.get(pairKey(a, b)) ?? measured.get(pairKey(b, a));
      return v;
    },
    predictedRtt: (a, b) => {
      if (a === b) return 0;
      const m = measured.get(pairKey(a, b)) ?? measured.get(pairKey(b, a));
      if (m !== undefined) return m;
      const p = predicted.get(pairKey(a, b)) ?? predicted.get(pairKey(b, a));
      return p === undefined ? Infinity : p;
    },
  };

  // --- /atlas fixtures (raw snake_case rows feasible() consumes) ---------------------------------
  /** @type {RawAtlasEntry[]} */
  const atlas = [
    { node_id: "us-a", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker", "asn:64500"] },
    { node_id: "us-b", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker", "asn:64500"] },
    { node_id: "eu-a", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker", "asn:64600"] },
    { node_id: "lone", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker"] },
    { node_id: "stale", cpu_cores: 32, mem_mb: 64000, running_jobs: 0, last_seen_secs: NOW - 9999, tags: ["docker"] }, // liveness fail
    { node_id: "small", cpu_cores: 1, mem_mb: 256, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker"] }, // headroom fail (needs 2c/512m)
    { node_id: "notag", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["highmem"] }, // tag fail (needs docker)
    { node_id: "me", cpu_cores: 8, mem_mb: 16000, running_jobs: 0, last_seen_secs: NOW - 10, tags: ["docker"] }, // self → excluded
  ];

  // --- /history fixtures: us-a is a trusted veteran; eu-a fresh; lone unknown --------------------
  const histories = new Map([
    ["us-a", { jobs_hosted: 200, heartbeats_hosted: 5000 }],
    ["us-b", { jobs_hosted: 2, heartbeats_hosted: 10 }],
    ["eu-a", { jobs_hosted: 0, heartbeats_hosted: 0 }],
    ["lone", null],
  ]);

  // Synthetic scoreFn (no scorer.js dependency): latency-dominated, plus a small trust term so the
  // four-axis shape is exercised. Pure function of the candidate.
  /** @type {ScoreFn} */
  const scoreFn = (c) => {
    const latency = clamp01(1 - c.rttMs / 250);
    const h = c.history;
    const delivered = h ? (Number(h.jobs_hosted) || 0) + (Number(h.heartbeats_hosted) || 0) : 0;
    const trust = clamp01(Math.log1p(delivered) / Math.log1p(50));
    const benchFit = 0.5;
    const price = 0.5;
    const total = 0.7 * latency + 0.1 * benchFit + 0.15 * trust + 0.05 * price;
    return { score: total, parts: { latency, benchFit, trust, price }, benchFit: { source: "atlas", confidence: 0.5 }, benchmarkSuspect: false };
  };

  // ============================================================================================
  // 1) feasible() hard filter.
  // ============================================================================================
  const req1 = { payer: "me", k: 1, cpuCores: 2, memMb: 512, requireTags: ["docker"] };
  const feas = feasible(atlas, req1, graph, NOW);
  const feasIds = new Set(feas.map((c) => c.nodeId));
  assert(feasIds.has("us-a") && feasIds.has("us-b") && feasIds.has("eu-a") && feasIds.has("lone"), "the four good hosts are feasible");
  assert(!feasIds.has("stale"), "stale host excluded by liveness");
  assert(!feasIds.has("small"), "undersized host excluded by headroom");
  assert(!feasIds.has("notag"), "host missing required 'docker' tag excluded");
  assert(!feasIds.has("me"), "payer self excluded by default");
  // Candidate shape: capacity normalized, rttMeasured ground-truth where a direct sample exists.
  const cUsA = feas.find((c) => c.nodeId === "us-a");
  assert(cUsA && cUsA.capacity.cpuCores === 8 && cUsA.capacity.memMb === 16000, "candidate carries normalized capacity");
  assert(cUsA.rttMeasured === true && cUsA.rttMs === 5, "us-a uses the measured RTT (ground truth)");
  const cLone = feas.find((c) => c.nodeId === "lone");
  assert(cLone && cLone.rttMeasured === false && cLone.rttMs === 40, "lone uses the predicted RTT (no direct sample)");

  // allowSelf admits the payer; exclude removes a host.
  const feasSelf = feasible(atlas, { ...req1, allowSelf: true, exclude: ["eu-a"] }, graph, NOW);
  const selfIds = new Set(feasSelf.map((c) => c.nodeId));
  assert(selfIds.has("me"), "allowSelf admits the payer");
  assert(!selfIds.has("eu-a"), "exclude removes a host");

  // ============================================================================================
  // 2) beaconSeed determinism + sensitivity.
  // ============================================================================================
  const beacon = { height: 12345, hash: "deadbeefcafe" };
  const s1 = beaconSeed(beacon, { payer: "me", nonce: "job-1" });
  const s2 = beaconSeed(beacon, { payer: "me", nonce: "job-1" });
  const s3 = beaconSeed(beacon, { payer: "me", nonce: "job-2" });
  assert(s1 === s2, "beaconSeed is deterministic for identical inputs");
  assert(s1 !== s3, "beaconSeed changes with the request nonce");
  assert(Number.isInteger(s1) && s1 >= 0, "seed is a non-negative 32-bit integer");

  // ============================================================================================
  // 3) redundancyFor() — verify raises effectiveK; "none" leaves it at k.
  // ============================================================================================
  // Attach histories so coarseTrust is exercised.
  const feasH = feas.map((c) => ({ ...c, history: histories.get(c.nodeId) ?? null }));
  assert(redundancyFor(feasH, { payer: "me", k: 1, redundancy: "none" }) === 1, "redundancy=none keeps effectiveK=k");
  // best feasible trust = us-a (5200 delivered => ~saturated >= 0.6) so verify needs 2 replicas.
  assert(redundancyFor(feasH, { payer: "me", k: 1, redundancy: "verify" }) === 2, "verify with a high-trust pool => 2 replicas");
  // Pool of only low-trust hosts (eu-a fresh, lone unknown) => verify wants 3 replicas, but the pool
  // has only 2 hosts, so effectiveK is bounded by the pool size.
  const lowTrustPool = feasH.filter((c) => c.nodeId !== "us-a" && c.nodeId !== "us-b");
  assert(lowTrustPool.length === 2, "low-trust pool fixture has 2 hosts");
  assert(redundancyFor(lowTrustPool, { payer: "me", k: 1, redundancy: "verify" }) === 2, "verify wants 3 but is bounded by the 2-host pool size");
  // k dominates when larger than the policy minimum.
  assert(redundancyFor(feasH, { payer: "me", k: 4, redundancy: "verify" }) === 4, "explicit k>policy wins");

  // ============================================================================================
  // 4) select() — spread across vendors, low-RTT first, cap respected (k=3 balanced).
  // ============================================================================================
  const req4 = { payer: "me", k: 3, cpuCores: 2, memMb: 512, requireTags: ["docker"], redundancy: "verify", maxShare: 0.34, objective: "latency" };
  const tagged4 = tagCandidates(feasH, graph);
  const seed4 = beaconSeed(beacon, { ...req4, nonce: "n4" });
  const res4 = select(tagged4, req4, graph, seed4, scoreFn);
  const ids4 = res4.targets.map((t) => t.nodeId);
  assert(ids4.length === 3, `verify k=3 places 3 hosts, got ${ids4.length} (${ids4.join(",")})`);
  assert(ids4[0] === "us-a", `lowest-RTT host placed first, got ${ids4[0]}`);
  const ops4 = res4.targets.map((t) => t.groups.operator);
  assert(new Set(ops4).size === ops4.length, "all replicas on DISTINCT operators (verify)");
  // per-operator cap respected (cap = ceil(3*0.34)=2, and verify forces distinct anyway).
  const opCounts4 = {};
  for (const o of ops4) opCounts4[o] = (opCounts4[o] ?? 0) + 1;
  const cap4 = perGroupCap(3, 0.34);
  assert(Object.values(opCounts4).every((n) => n <= cap4), "no operator exceeds the per-group cap");

  // The diversity penalty must have teeth: with a STRONG wD, the correlated us-b (shares us-a's
  // asn+region+cluster) is pushed below the independent lone@40 for the second slot, even though
  // us-b is far closer (7ms vs 40ms). This is the "spread across vendors beats raw proximity when
  // diversity is weighted" guarantee. (At a low wD the closer us-b legitimately wins — that is the
  // tunable tradeoff, not a bug; here we prove the lever works.)
  const reqDiv = { ...req4, weights: { wL: 0.5, wB: 0.15, wT: 0.2, wP: 0.05, wD: 1.5 } };
  const resDiv = select(tagCandidates(feasH, graph), reqDiv, graph, seed4, scoreFn);
  const idsDiv = resDiv.targets.map((t) => t.nodeId);
  assert(idsDiv[0] === "us-a", "strong-diversity run still anchors on the best host");
  assert(idsDiv[1] === "lone", `strong wD makes the independent host beat the correlated near host, got ${idsDiv[1]}`);

  // ============================================================================================
  // 5) select() — short plan when the pool is too small / too constrained.
  // ============================================================================================
  // Only two distinct operators (us-a, us-b share asn/region but are distinct operators; lone+eu-a)
  // — ask for k=6 under verify => cannot satisfy 6 distinct operators from 4 hosts.
  const req5 = { payer: "me", k: 6, cpuCores: 2, memMb: 512, requireTags: ["docker"], redundancy: "verify" };
  const seed5 = beaconSeed(beacon, { ...req5, nonce: "n5" });
  const res5 = select(tagged4, req5, graph, seed5, scoreFn);
  assert(res5.targets.length === 4, `verify across 4 distinct operators places 4, got ${res5.targets.length}`);
  assert(res5.shortfall > 0, "an unsatisfiable replica count is reported as a shortfall");
  assert(res5.shortfall === redundancyFor(tagged4, req5) - res5.targets.length, "shortfall = effectiveK - placed");

  // ============================================================================================
  // 6) select() — strict spread hard-rejects correlated hosts (perGroup forces independence).
  // ============================================================================================
  // k=4, maxShare tiny so cap=1 on every group: us-a and us-b cannot both be chosen (share asn/region).
  const req6 = { payer: "me", k: 4, cpuCores: 2, memMb: 512, requireTags: ["docker"], redundancy: "verify", maxShare: 0.01 };
  const seed6 = beaconSeed(beacon, { ...req6, nonce: "n6" });
  const res6 = select(tagged4, req6, graph, seed6, scoreFn);
  const ids6 = res6.targets.map((t) => t.nodeId);
  assert(!(ids6.includes("us-a") && ids6.includes("us-b")), "perGroup=1 hard-excludes the correlated us-a/us-b pair together");
  // every rejected host carries a reason.
  for (const rj of res6.rejected) {
    assert(typeof rj.reason === "string" && rj.reason.length > 0, `rejected host ${rj.nodeId} has a reason`);
  }

  // ============================================================================================
  // 7) select() — determinism: same seed → same plan; selection="weighted" still valid.
  // ============================================================================================
  const resA = select(tagCandidates(feasH, graph), req4, graph, seed4, scoreFn);
  const resB = select(tagCandidates(feasH, graph), req4, graph, seed4, scoreFn);
  assert(
    resA.targets.map((t) => t.nodeId).join(",") === resB.targets.map((t) => t.nodeId).join(","),
    "select is deterministic given a fixed seed",
  );
  const reqW = { ...req4, selection: "weighted", temperature: 0.1 };
  const resW = select(tagCandidates(feasH, graph), reqW, graph, seed4, scoreFn);
  assert(resW.targets.length === 3, "weighted selection still fills the cohort");
  assert(new Set(resW.targets.map((t) => t.groups.operator)).size === 3, "weighted selection still honors distinct operators");

  // ============================================================================================
  // 8) cohort: colocate prefers same-region; spread prefers fresh regions.
  // ============================================================================================
  // colocate, k=2, no verify (so operators may repeat) → after us-a, the same-region us-b should be
  // preferred over the far eu-a despite the diversity penalty, because the colocate RTT bonus + the
  // strong latency term dominate.
  const reqColo = { payer: "me", k: 2, cpuCores: 2, memMb: 512, requireTags: ["docker"], redundancy: "none", cohort: "colocate", objective: "latency", maxShare: 1 };
  const seedC = beaconSeed(beacon, { ...reqColo, nonce: "nc" });
  const resColo = select(tagCandidates(feasH, graph), reqColo, graph, seedC, scoreFn);
  const idsColo = resColo.targets.map((t) => t.nodeId);
  assert(idsColo[0] === "us-a", "colocate still anchors on the best host");
  assert(idsColo[1] === "us-b", `colocate prefers the same-region neighbour, got ${idsColo[1]}`);

  return { ok: true, checks };
}
