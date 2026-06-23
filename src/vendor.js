/**
 * Vendor / diversity / risk model — pure functions over candidates + the chosen set. No I/O.
 *
 * Stops a job (and a user's recent jobs) from piling onto one operator or one correlated cluster.
 * Provides the grouping keys, the concentration caps, and the contextual diversity penalty the
 * placer applies during selection. This is the §4 ("vendor / risk model") layer of
 * docs/placement-design.md, implemented at the app layer; it reads only public facts (the graph's
 * regions + the candidate's profile/atlas tags) and never performs I/O.
 *
 * The CE client is dependency-injected into the *placer* (the only I/O module). vendor.js is pure:
 * the placer passes it candidates already enriched with `.profile`/`.capacity`/`.groups`. Where a
 * vendor function needs to look at a CE client it does not — by design these are deterministic
 * functions of their arguments so the whole diversity policy is unit-testable offline (see
 * `__selftest` at the bottom, which runs on synthetic netgraph/atlas/history fixtures).
 *
 * CONTRACT (frozen — see docs/placement-design.md §4 and "Module contracts"):
 *
 *   groupKeys(candidate, graph)              -> { operator, asn, region, cluster }
 *       operator: best available controller proxy (today: candidate.nodeId; later: on-chain owner)
 *       asn:      network provider hint (today: profile/runtime hint or an "asn:<x>" tag, else "unknown")
 *       region:   String(graph.regionOf(nodeId)); latency cluster = same DC/LAN proxy
 *       cluster:  union-find bucket over {operator,asn,region} — the broadest correlation group
 *   tagCandidates(candidates, graph)         -> Candidate[]   // attach .groups to each (returns enriched list)
 *   perGroupCap(effectiveK, maxShare)        -> number        // ceil(effectiveK * maxShare), min 1
 *   diversityPenalty(candidate, chosen, req) -> number        // [0,~2]: groupLoad(c,chosen) + recentLoad(c,req.recentPlacements)
 *   violatesCap(candidate, chosen, caps)     -> string|null   // offending group key if adding c breaks a hard cap, else null
 *       caps: { perGroup:number, requireDistinctOperator:boolean }  // distinct-operator enforced for redundancy="verify"
 *   clusterOf(candidates)                    -> Map<NodeId,string>  // union-find label per node over operator/asn/region overlap
 *
 * groupLoad  = fraction of `chosen` already sharing ANY of c's groups (operator/asn/region/cluster).
 * recentLoad = this payer's recentPlacements concentration on c.operator (0 if none) — spreads a
 *              user's exposure across jobs over time, not just within one job.
 *
 * @module vendor
 */

/** @typedef {import("./types.js").Candidate} Candidate */
/** @typedef {import("./types.js").NodeId} NodeId */
/** @typedef {import("./types.js").NodeProfile} NodeProfile */
/** @typedef {import("./types.js").PlacementRequest} PlacementRequest */
/** @typedef {import("./graph.js").Graph} Graph */

/** Sentinel grouping value when a key cannot be derived from public facts. */
const UNKNOWN = "unknown";

// ----------------------------------------------------------------------------
// Grouping keys (§4).
// ----------------------------------------------------------------------------

/**
 * Pull the candidate's tag list from whatever is populated. Prefer the live atlas capacity (which
 * the graph normalizes to `tags`), then the candidate's own capacity, then [].
 * @param {Candidate} candidate
 * @param {Graph} [graph]
 * @returns {string[]}
 */
function tagsOf(candidate, graph) {
  if (candidate && candidate.capacity && Array.isArray(candidate.capacity.tags)) {
    return candidate.capacity.tags;
  }
  if (graph && candidate && candidate.nodeId) {
    const cap = graph.capacityOf(candidate.nodeId);
    if (cap && Array.isArray(cap.tags)) return cap.tags;
  }
  return [];
}

/**
 * Best-available ASN (network-provider) proxy from public facts, in priority order:
 *  1. an explicit profile runtime hint (future: `profile.runtime.asn`),
 *  2. an `asn:<x>` tag on the atlas entry (operators may self-tag; correlated-outage proxy),
 *  3. "unknown".
 * ASN is the weakest grouping signal today; it upgrades for free once NodeProfile/runtime carries
 * it. Treated purely as a correlation bucket, never trusted for authorization.
 * @param {Candidate} candidate
 * @param {Graph} [graph]
 * @returns {string}
 */
function asnOf(candidate, graph) {
  const profile = candidate ? candidate.profile : null;
  if (profile && profile.runtime && typeof /** @type {any} */ (profile.runtime).asn === "string") {
    const a = /** @type {any} */ (profile.runtime).asn.trim();
    if (a) return a;
  }
  for (const tag of tagsOf(candidate, graph)) {
    if (typeof tag === "string" && tag.toLowerCase().startsWith("asn:")) {
      const a = tag.slice(4).trim();
      if (a) return a;
    }
  }
  return UNKNOWN;
}

/**
 * Best-available operator (controller / owner) proxy. Today the node id IS the operator key (one
 * key per node). When `/history` or an on-chain owner record is folded onto the candidate it should
 * win, so two nodes under one owner collapse to one operator group (verification across them proves
 * nothing). We look for `candidate.history.owner` / `candidate.history.operator` first, then the
 * node id.
 * @param {Candidate} candidate
 * @returns {string}
 */
function operatorOf(candidate) {
  const h = candidate ? candidate.history : null;
  if (h && typeof h === "object") {
    const owner = /** @type {any} */ (h).owner ?? /** @type {any} */ (h).operator;
    if (typeof owner === "string" && owner.trim()) return owner.trim();
  }
  return candidate && candidate.nodeId ? candidate.nodeId : UNKNOWN;
}

/**
 * Grouping keys for one candidate. Region uses the graph's O(1) `regionOf` (a measured-latency
 * cluster = a same-DC/LAN correlation proxy); `-1` (unknown region) maps to a per-node region key so
 * region-less nodes never accidentally share a region with each other.
 * @param {Candidate} candidate
 * @param {Graph} graph
 * @returns {{operator:string, asn:string, region:string, cluster:string}}
 */
export function groupKeys(candidate, graph) {
  const operator = operatorOf(candidate);
  const asn = asnOf(candidate, graph);
  const ridx = graph && typeof graph.regionOf === "function" ? graph.regionOf(candidate.nodeId) : -1;
  // -1 = no measured region; give it a node-unique key so two unplaced nodes are NOT co-region.
  const region = ridx >= 0 ? `r${ridx}` : `r?:${candidate.nodeId}`;
  // cluster is filled by clusterOf() union-find when the whole candidate set is known; for a lone
  // candidate it degenerates to the {operator|asn|region} composite (its own broadest bucket).
  const cluster = `${operator}|${asn}|${region}`;
  return { operator, asn, region, cluster };
}

/**
 * Attach `.groups` to every candidate and resolve the union-find `cluster` label across the whole
 * pool (so transitively-correlated candidates share one cluster). Returns a NEW array of NEW
 * candidate objects (does not mutate inputs) — the placer wants a stable enriched pool.
 * @param {Candidate[]} candidates
 * @param {Graph} graph
 * @returns {Candidate[]}
 */
export function tagCandidates(candidates, graph) {
  const list = Array.isArray(candidates) ? candidates : [];
  // First pass: per-candidate keys (operator/asn/region + provisional cluster).
  const enriched = list.map((c) => ({ ...c, groups: groupKeys(c, graph) }));
  // Second pass: union-find collapses any two candidates sharing operator OR asn OR region into one
  // cluster label, so the broadest correlation bucket is transitive.
  const cluster = clusterOf(enriched);
  for (const c of enriched) {
    const label = cluster.get(c.nodeId);
    if (label && c.groups) c.groups.cluster = label;
  }
  return enriched;
}

// ----------------------------------------------------------------------------
// Concentration caps (§4).
// ----------------------------------------------------------------------------

/**
 * Max candidates from any single group allowed in one job: `ceil(effectiveK * maxShare)`, floored at
 * 1 (a group may always hold at least one host, else nothing is placeable). Default maxShare 0.34 ⇒
 * no group exceeds ~1/3, so a k≥3 job spans `ceil(1/maxShare)` independent operators.
 * @param {number} effectiveK
 * @param {number} maxShare  fraction in (0,1]
 * @returns {number}
 */
export function perGroupCap(effectiveK, maxShare) {
  const k = Number.isFinite(effectiveK) && effectiveK > 0 ? effectiveK : 1;
  // A non-finite share defaults to 0.34; a degenerate non-positive share floors the cap to 1 (the
  // minimum admissible — every group may always hold one host); a share > 1 clamps to 1.
  let share = Number.isFinite(maxShare) ? maxShare : 0.34;
  if (share <= 0) return 1;
  if (share > 1) share = 1;
  return Math.max(1, Math.ceil(k * share));
}

/**
 * How many of `chosen` already share at least one group with `candidate`, per group key. Used both
 * by the soft penalty (groupLoad) and the hard cap (violatesCap).
 * @param {Candidate} candidate
 * @param {Candidate[]} chosen
 * @returns {{operator:number, asn:number, region:number, cluster:number, any:number}}
 */
function groupCounts(candidate, chosen) {
  const cg = candidate.groups;
  const counts = { operator: 0, asn: 0, region: 0, cluster: 0, any: 0 };
  if (!cg) return counts;
  for (const ch of chosen) {
    const g = ch.groups;
    if (!g) continue;
    let shared = false;
    if (g.operator === cg.operator) {
      counts.operator++;
      shared = true;
    }
    // asn/region/cluster only count as correlation when the value is a real signal (not "unknown"
    // and not a per-node region sentinel) — otherwise every region-less / asn-less node would look
    // mutually correlated and the caps would over-fire.
    if (cg.asn !== UNKNOWN && g.asn === cg.asn) {
      counts.asn++;
      shared = true;
    }
    if (!cg.region.startsWith("r?:") && g.region === cg.region) {
      counts.region++;
      shared = true;
    }
    if (g.cluster === cg.cluster) {
      counts.cluster++;
      shared = true;
    }
    if (shared) counts.any++;
  }
  return counts;
}

// ----------------------------------------------------------------------------
// Soft penalty (§2 wD term, §4).
// ----------------------------------------------------------------------------

/**
 * Contextual diversity penalty in roughly [0, 2]:
 *
 *   diversityPenalty = groupLoad + recentLoad
 *     groupLoad  = (# of `chosen` sharing ANY group with c) / max(1, |chosen|)   // within-job spread
 *     recentLoad = recentPlacements[c.operator] / Σ recentPlacements             // across-user spread
 *
 * Subtracted (times `wD`) from the live score during selection, so the second-best-but-DIFFERENT
 * host beats the second-best-but-SAME host. Returns 0 for the first pick (empty `chosen`) when the
 * payer has no recent history, so an unconstrained job is unaffected.
 * @param {Candidate} candidate
 * @param {Candidate[]} chosen
 * @param {PlacementRequest} req
 * @returns {number}
 */
export function diversityPenalty(candidate, chosen, req) {
  const chosenList = Array.isArray(chosen) ? chosen : [];
  const counts = groupCounts(candidate, chosenList);
  const groupLoad = chosenList.length > 0 ? counts.any / chosenList.length : 0;

  let recentLoad = 0;
  const recent = req && req.recentPlacements;
  if (recent && typeof recent === "object") {
    let total = 0;
    for (const v of Object.values(recent)) {
      const n = Number(v);
      if (Number.isFinite(n) && n > 0) total += n;
    }
    if (total > 0) {
      const op = candidate.groups ? candidate.groups.operator : operatorOf(candidate);
      const mine = Number(recent[op]);
      if (Number.isFinite(mine) && mine > 0) recentLoad = mine / total;
    }
  }
  return groupLoad + recentLoad;
}

// ----------------------------------------------------------------------------
// Hard caps (§5b).
// ----------------------------------------------------------------------------

/**
 * Would adding `candidate` to `chosen` break a HARD constraint? Returns the offending group key
 * (`"operator" | "asn" | "region" | "cluster"`) so the placer can record a precise `rejected`
 * reason / decide which cap to relax, or null if the candidate is admissible.
 *
 * Two hard gates:
 *  1. distinct-operator (redundancy="verify"): a replica may NOT share an operator with any chosen
 *     replica — verification across one operator's two boxes proves nothing. Checked first because
 *     it is non-negotiable and independent of the count cap.
 *  2. per-group count cap: adding c must not push the count in any of its groups to `> caps.perGroup`.
 *     Real-signal groups only (operator always; asn/region/cluster only when not a sentinel) so
 *     region-less / asn-less hosts are not spuriously capped together.
 *
 * @param {Candidate} candidate
 * @param {Candidate[]} chosen
 * @param {{perGroup:number, requireDistinctOperator:boolean}} caps
 * @returns {string|null}
 */
export function violatesCap(candidate, chosen, caps) {
  const chosenList = Array.isArray(chosen) ? chosen : [];
  const perGroup = caps && Number.isFinite(caps.perGroup) ? caps.perGroup : Infinity;
  const requireDistinctOperator = !!(caps && caps.requireDistinctOperator);
  const counts = groupCounts(candidate, chosenList);
  const cg = candidate.groups;

  // 1. distinct-operator (verify) — strongest, count-independent.
  if (requireDistinctOperator && counts.operator > 0) return "operator";

  // 2. per-group count caps. `counts.x` is how many chosen ALREADY share group x; adding c makes it
  //    counts.x + 1, which must stay <= perGroup.
  if (counts.operator + 1 > perGroup) return "operator";
  if (cg && cg.asn !== UNKNOWN && counts.asn + 1 > perGroup) return "asn";
  if (cg && !cg.region.startsWith("r?:") && counts.region + 1 > perGroup) return "region";
  if (counts.cluster + 1 > perGroup) return "cluster";

  return null;
}

// ----------------------------------------------------------------------------
// Union-find clustering (§4).
// ----------------------------------------------------------------------------

/**
 * Union-find over the candidate pool: any two candidates that share an operator, a (real) asn, or a
 * (real) region are joined into one cluster. The returned Map gives each node a stable cluster label
 * (the representative node id of its set), so transitively-correlated hosts collapse to one bucket —
 * the broadest correlation group used by `groups.cluster`.
 *
 * Candidates must already carry `.groups` (call after `groupKeys`/`tagCandidates`); if a candidate
 * lacks groups it is treated as its own singleton cluster.
 * @param {Candidate[]} candidates
 * @returns {Map<NodeId,string>}
 */
export function clusterOf(candidates) {
  const list = Array.isArray(candidates) ? candidates : [];
  /** @type {Map<NodeId,NodeId>} */
  const parent = new Map();

  /** @param {NodeId} x */
  const find = (x) => {
    let root = x;
    while (parent.get(root) !== root) root = /** @type {NodeId} */ (parent.get(root));
    // path-compress
    let cur = x;
    while (parent.get(cur) !== root) {
      const next = /** @type {NodeId} */ (parent.get(cur));
      parent.set(cur, root);
      cur = next;
    }
    return root;
  };
  /** @param {NodeId} a @param {NodeId} b */
  const union = (a, b) => {
    const ra = find(a);
    const rb = find(b);
    if (ra !== rb) parent.set(ra, rb);
  };

  for (const c of list) parent.set(c.nodeId, c.nodeId);

  // Index by each real grouping value, unioning every node that shares it.
  /** @type {Map<string, NodeId>} */
  const firstByKey = new Map();
  /** @param {string} ns @param {string|undefined} val @param {NodeId} node */
  const link = (ns, val, node) => {
    if (!val) return;
    const key = `${ns}=${val}`;
    const prev = firstByKey.get(key);
    if (prev === undefined) firstByKey.set(key, node);
    else union(prev, node);
  };

  for (const c of list) {
    const g = c.groups ?? groupKeys(c, /** @type {any} */ (undefined));
    link("op", g.operator, c.nodeId);
    if (g.asn && g.asn !== UNKNOWN) link("asn", g.asn, c.nodeId);
    if (g.region && !g.region.startsWith("r?:")) link("region", g.region, c.nodeId);
  }

  /** @type {Map<NodeId,string>} */
  const out = new Map();
  for (const c of list) out.set(c.nodeId, find(c.nodeId));
  return out;
}

// ----------------------------------------------------------------------------
// Offline self-test (synthetic fixtures — no live node, no I/O).
// ----------------------------------------------------------------------------

/**
 * Deterministic offline verification of the vendor/diversity policy on synthetic fixtures. Runs the
 * same primitives the placer uses (groupKeys/tagCandidates/perGroupCap/violatesCap/diversityPenalty)
 * and a tiny greedy selection loop wired exactly as placer.js §5 will wire them, then asserts the
 * behaviours the task calls out:
 *   - spreads across vendors / operators,
 *   - never puts > perGroupCap on one operator,
 *   - prefers the lower-RTT host when the diversity penalty is equal,
 *   - requires DISTINCT operators per replica when redundancy="verify".
 *
 * Uses a stub Graph (only `regionOf`/`capacityOf` are needed) so it stays offline and independent of
 * graph.js's embedding.
 * @returns {{ ok: true, checks: number }}
 */
export function __selftest() {
  let checks = 0;
  /** @param {boolean} cond @param {string} msg */
  const assert = (cond, msg) => {
    checks++;
    if (!cond) throw new Error(`vendor.__selftest FAILED: ${msg}`);
  };

  // --- Stub graph: 3 operators across 2 regions, plus an atlas-style tag carrier ----------------
  // us-a, us-b => region 0 (same DC); eu-a => region 1; lone => no region (-1).
  const regionMap = new Map([
    ["us-a", 0],
    ["us-b", 0],
    ["eu-a", 1],
    ["lone", -1],
  ]);
  const capMap = new Map([
    ["us-a", { nodeId: "us-a", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: 0, tags: ["docker", "asn:64500"] }],
    ["us-b", { nodeId: "us-b", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: 0, tags: ["docker", "asn:64500"] }],
    ["eu-a", { nodeId: "eu-a", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: 0, tags: ["docker", "asn:64600"] }],
    ["lone", { nodeId: "lone", cpuCores: 8, memMb: 16000, runningJobs: 0, lastSeenSecs: 0, tags: ["docker"] }],
  ]);
  const graph = {
    regionOf: (n) => (regionMap.has(n) ? regionMap.get(n) : -1),
    capacityOf: (n) => capMap.get(n),
  };

  /** @param {string} id @param {number} rttMs */
  const mk = (id, rttMs) => ({
    nodeId: id,
    capacity: capMap.get(id),
    profile: null,
    history: null,
    rttMs,
    rttMeasured: true,
  });

  // --- groupKeys / tagCandidates ---------------------------------------------------------------
  const pool = tagCandidates([mk("us-a", 5), mk("us-b", 7), mk("eu-a", 90), mk("lone", 40)], graph);
  const byId = new Map(pool.map((c) => [c.nodeId, c]));

  assert(byId.get("us-a").groups.operator === "us-a", "operator key = node id");
  assert(byId.get("us-a").groups.asn === "64500", "asn parsed from asn: tag");
  assert(byId.get("eu-a").groups.asn === "64600", "eu-a distinct asn");
  assert(byId.get("lone").groups.asn === UNKNOWN, "no asn tag => unknown");
  assert(byId.get("us-a").groups.region === "r0" && byId.get("us-b").groups.region === "r0", "us-a/us-b co-region");
  assert(byId.get("eu-a").groups.region === "r1", "eu-a region 1");
  assert(byId.get("lone").groups.region.startsWith("r?:"), "region-less node gets unique region sentinel");

  // us-a & us-b share asn AND region => same union-find cluster; eu-a & lone are separate.
  assert(byId.get("us-a").groups.cluster === byId.get("us-b").groups.cluster, "us-a/us-b cluster together");
  assert(byId.get("us-a").groups.cluster !== byId.get("eu-a").groups.cluster, "eu-a in a different cluster");
  assert(byId.get("lone").groups.cluster !== byId.get("us-a").groups.cluster, "lone is its own cluster");

  // --- perGroupCap ------------------------------------------------------------------------------
  assert(perGroupCap(3, 0.34) === 2, "ceil(3*0.34)=2");
  assert(perGroupCap(1, 0.34) === 1, "k=1 cap is 1");
  assert(perGroupCap(9, 0.34) === 4, "ceil(9*0.34)=4");
  assert(perGroupCap(3, 0) === 1, "degenerate maxShare floored to cap 1");

  // --- diversityPenalty: prefer DIFFERENT group; tie => RTT decides upstream --------------------
  // After choosing us-a, us-b shares operator? no (distinct ids) but shares asn+region => penalized;
  // eu-a shares nothing => penalty 0; lone shares nothing => penalty 0.
  const chosen1 = [byId.get("us-a")];
  const penUsB = diversityPenalty(byId.get("us-b"), chosen1, {});
  const penEuA = diversityPenalty(byId.get("eu-a"), chosen1, {});
  const penLone = diversityPenalty(byId.get("lone"), chosen1, {});
  assert(penUsB > 0, "us-b penalized for sharing asn/region/cluster with chosen us-a");
  assert(penEuA === 0, "eu-a unpenalized (fully independent of us-a)");
  assert(penLone === 0, "lone unpenalized (fully independent of us-a)");
  assert(penUsB > penEuA, "correlated host is penalized more than an independent one");

  // recentLoad: payer has placed heavily on eu-a recently => eu-a gains a penalty even when
  // independent within THIS job.
  const recentReq = { recentPlacements: { "eu-a": 8, "us-a": 2 } };
  const penEuARecent = diversityPenalty(byId.get("eu-a"), [], recentReq);
  assert(Math.abs(penEuARecent - 0.8) < 1e-9, "recentLoad = 8/10 for eu-a across-user concentration");
  assert(diversityPenalty(byId.get("lone"), [], recentReq) === 0, "lone has no recent concentration");

  // --- violatesCap: per-group count + distinct operator (verify) --------------------------------
  // cap perGroup=1 (e.g. effectiveK small, strict spread): once us-a chosen, us-b violates on
  // asn/region/cluster (shares them) though operator differs.
  const strictCaps = { perGroup: 1, requireDistinctOperator: false };
  assert(violatesCap(byId.get("us-b"), [byId.get("us-a")], strictCaps) !== null, "us-b breaks perGroup=1 cap vs us-a");
  assert(violatesCap(byId.get("eu-a"), [byId.get("us-a")], strictCaps) === null, "eu-a fits under perGroup=1 (independent)");

  // distinct-operator (verify): the SAME operator is rejected even if perGroup is loose. Build a
  // second node under the same owner via history.owner to exercise the operator-collapse path.
  const dupOwner = { ...mk("us-d", 6), history: { owner: "us-a" } };
  const dupTagged = tagCandidates([byId.get("us-a"), dupOwner], graph);
  const usaT = dupTagged[0];
  const dupT = dupTagged[1];
  assert(dupT.groups.operator === "us-a", "history.owner collapses us-d into operator us-a");
  const verifyCaps = { perGroup: 99, requireDistinctOperator: true };
  assert(violatesCap(dupT, [usaT], verifyCaps) === "operator", "verify rejects same-operator replica");
  assert(violatesCap(byId.get("eu-a"), [byId.get("us-a")], verifyCaps) === null, "verify admits distinct operator");

  // --- end-to-end greedy spread (mirrors placer §5) ---------------------------------------------
  // effectiveK=3, balanced cap. Live score = latency only here (1 - rtt/250), minus wD*penalty.
  // Expect: distinct operators, never > cap on one operator, low-RTT preferred among independents.
  const effectiveK = 3;
  const cap = perGroupCap(effectiveK, 0.34); // = 2
  const wD = 0.15;
  const candPool = tagCandidates(
    [mk("us-a", 5), mk("us-b", 7), mk("eu-a", 90), mk("lone", 40)],
    graph,
  );
  const latency = (c) => 1 - c.rttMs / 250;
  const picked = [];
  while (picked.length < effectiveK) {
    let best = null;
    let bestScore = -Infinity;
    for (const c of candPool) {
      if (picked.includes(c)) continue;
      if (violatesCap(c, picked, { perGroup: cap, requireDistinctOperator: true }) !== null) continue;
      const live = latency(c) - wD * diversityPenalty(c, picked, {});
      if (live > bestScore) {
        bestScore = live;
        best = c;
      }
    }
    if (!best) break;
    picked.push(best);
  }
  const pickedIds = picked.map((c) => c.nodeId);
  assert(pickedIds[0] === "us-a", "lowest-RTT host picked first");
  const ops = picked.map((c) => c.groups.operator);
  assert(new Set(ops).size === ops.length, "all picks are on DISTINCT operators (verify spread)");
  // No operator appears more than the cap.
  const opCounts = {};
  for (const o of ops) opCounts[o] = (opCounts[o] ?? 0) + 1;
  assert(Object.values(opCounts).every((n) => n <= cap), "no operator exceeds perGroupCap");
  // The independent low-RTT host (lone@40) outranks the correlated us-b@7 on the SECOND pick:
  // us-b's full groupLoad penalty (shares asn+region+cluster with us-a) sinks it below lone.
  assert(pickedIds[1] === "lone", "independent lone beats correlated us-b on the soft penalty");

  // --- strict-cap pass: the HARD per-group cap (=1) excludes correlated hosts outright ----------
  // Same pool, perGroup=1 so any second host sharing us-a's asn/region/cluster is hard-rejected.
  const strictPicked = [];
  while (strictPicked.length < effectiveK) {
    let best = null;
    let bestScore = -Infinity;
    for (const c of candPool) {
      if (strictPicked.includes(c)) continue;
      if (violatesCap(c, strictPicked, { perGroup: 1, requireDistinctOperator: true }) !== null) continue;
      const live = latency(c) - wD * diversityPenalty(c, strictPicked, {});
      if (live > bestScore) {
        bestScore = live;
        best = c;
      }
    }
    if (!best) break;
    strictPicked.push(best);
  }
  const strictIds = strictPicked.map((c) => c.nodeId);
  assert(!strictIds.includes("us-b"), "perGroup=1 hard-excludes us-b (shares asn/region/cluster with us-a)");
  assert(
    strictIds.includes("us-a") && strictIds.includes("eu-a") && strictIds.includes("lone"),
    "strict spread selects the three mutually-independent operators",
  );

  return { ok: true, checks };
}
