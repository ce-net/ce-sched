# ce-sched placement design

How `ce-sched` picks the **best targets** for a job from the live Fabric Map. This is the P3
("placement") layer of `ce/docs/compute-fabric.md`, implemented at the app layer. It consumes the
P0/P1 substrate (`/netgraph` measured RTT + Vivaldi coordinates) and the P2 substrate (signed
`NodeProfile`s from `ce-bench`, folded into `/atlas`), and never modifies the node.

Two outputs are possible from one request:

1. **Single placement** — `k = 1`: the one best host for a one-shot job.
2. **Cohort placement** — `k > 1`: several hosts for a parallel / redundant / multi-stage job, where
   the hosts' relationship to *each other* (co-location vs spread) also matters.

The pipeline is: **gather → candidate-build → score → constrain & select → (beacon tie-break) → plan**.

---

## 0. Inputs (read-substrate only)

| Source | Endpoint | Used for |
|---|---|---|
| Measured RTT edges | `GET /netgraph` | latency to the payer, region clustering, cohort co-location |
| Live capacity + tags + (later) profile | `GET /atlas` | hard feasibility, benchmark fit, free CPU/mem |
| Reputation facts | `GET /history/:node_id` | trust weight, redundancy requirement, benchmark cross-check |
| Public randomness | `GET /beacon` | anti-collusion tie-break / selection seed |
| Signed `NodeProfile` | folded into `/atlas` (P2) | GFLOPS / VRAM / TFLOPS / tokens-per-sec matching |

The **payer's own node id** (the latency origin) is supplied by the caller in the
`PlacementRequest` (`payer`); it is the node whose `/netgraph` vantage anchors "near me".

`NodeProfile` is not on the wire yet. Until it is, the scorer's benchmark term uses the coarse
atlas signals (`cpu_cores`, `mem_mb`, tags like `gpu`/`manycore`/`highmem`) as a fallback and marks
`benchFit.source = "atlas"`. The contract is identical; only the data source upgrades. See
`docs/node-profile-spec.md`.

---

## 1. Candidate build (hard feasibility filter)

A node from `/atlas` becomes a `Candidate` only if it passes every **hard constraint**. Hard
constraints are pass/fail — they never trade off against score:

1. **Liveness** — `now - lastSeenSecs <= req.maxStaleSecs` (default 180s; atlas broadcasts every 60s).
2. **Resource headroom** — `cpuCores - estimatedUsed >= req.cpuCores` and
   `memMb (available) >= req.memMb`. With a `NodeProfile`, `memory.available_mb` is authoritative;
   without one, fall back to `mem_mb` minus a running-jobs heuristic.
3. **Required tags** — every tag in `req.requireTags` is present (e.g. `docker`, `gpu`, `x86_64`).
4. **Capability floor** — if the job declares a profile floor (`req.minGflops`, `req.minVramMb`,
   `req.minTokensPerSec`), the candidate's measured profile must meet it. No profile + a floor that
   *requires* measured data ⇒ excluded when `req.requireProfile` is true; otherwise admitted with a
   trust/redundancy penalty (it is unverified hardware).
5. **Reachability** — finite `predictedRtt(payer, candidate)`; an unreachable node cannot serve.
6. **Self/exclusion** — not in `req.exclude`, and (unless `req.allowSelf`) not the payer itself.

Survivors carry their raw signals forward: predicted RTT, measured RTT (if any), profile (or atlas
fallback), `/history` stats, region id, and operator/ASN/region grouping keys (see §4).

---

## 2. The scoring function

Each candidate gets a scalar **score ∈ [0, 1]**, higher = better, computed from five normalized
sub-scores blended by weights. Weights live in `req.weights` and default per `req.objective`
(`"latency" | "throughput" | "balanced" | "cheap"`). All sub-scores are in `[0, 1]` so weights are
interpretable and the blend is a convex combination.

```
score(c) = wL·latency(c)
         + wB·benchFit(c)
         + wT·trust(c)
         + wP·price(c)
         - wD·diversityPenalty(c | alreadyChosen)
```

`diversityPenalty` is **contextual** — it depends on what has already been selected — so it is
applied during selection (§5), not in the static pre-score. The static pre-score is the first four
terms; the selector re-ranks the remaining pool after each pick by subtracting the live penalty.

### 2.1 latency(c) — minimize RTT to the payer

```
rtt   = measuredRtt(payer, c) ?? predictedRtt(payer, c)      // ms, ground truth preferred
latency(c) = clamp01( 1 - rtt / req.rttSoftCapMs )           // default soft cap 250 ms
```

A direct measured sample is always preferred over the embedding prediction (it is ground truth).
Beyond the soft cap latency contributes 0 but the node is still feasible (it passed reachability);
the cap just flattens far hosts so they win only on other axes.

### 2.2 benchFit(c) — match the job's resource profile

Job declares a **profile demand vector** (`req.demand`): which axes matter and how much. benchFit is
the demand-weighted, saturating match of the candidate's measured profile to the demand:

```
axes = { gflops, memBwGbps, vramMb, fp16Tflops, tokensPerSec, diskReadMbps, diskWriteMbps }
for each axis a with demand d_a > 0 and a target t_a (the "enough" point for this job):
    fit_a = clamp01( profile[a] / t_a )         // 1.0 once the host clears the bar; no reward past it
benchFit(c) = Σ d_a·fit_a / Σ d_a              // demand-weighted mean, in [0,1]
```

Saturating (`clamp01`) is deliberate: a job that needs 8 GB VRAM gets no extra credit for a 80 GB
card on the *fit* axis — that surplus matters only for packing more jobs, handled by capacity, not
fit. If the candidate has no profile, `benchFit` uses the atlas fallback mapping
(tags/cores → coarse axis estimates) and the scorer records `source: "atlas"` and a confidence < 1
that the trust term consumes (unverified hardware is discounted).

### 2.3 trust(c) — reputation weight from /history

```
delivered = jobs_hosted + heartbeats_hosted          // proven work
recent    = recency-weighted slice (recent_earned vs earned)
trust(c)  = clamp01( log1p(delivered) / log1p(req.trustSaturation)   // default saturation 50
                     · recencyBoost
                     · profileConfidence )            // unverified hardware (atlas-only) discounted
```

Trust saturates (log curve): the gap between a brand-new host and a 10-job host is large; between a
50-job and a 500-job host it is small. A host that *claims* a huge profile but whose
`earned`/`jobs_hosted` is implausibly low for that claim is flagged (`benchmarkSuspect = true`) and
its trust is floored toward 0 — this is the compute-fabric.md §9 "card claiming throughput far above
delivered work" cross-check, done at the app layer from public facts.

### 2.4 price(c) — cheaper is better, but it is a soft axis

If the candidate advertises a price (future `NodeProfile`/signal field) the scorer normalizes it
against `req.priceCapBaseUnits` (a base-unit **string**, parsed to `BigInt`, never a float):

```
price(c) = clamp01( 1 - Number(askBaseUnits) / Number(req.priceCapBaseUnits) )
```

Money is integer base units end to end. The only float is the final `[0,1]` ratio used purely for
ranking — it never represents an amount and is never written back as money. If no price is
advertised, `price(c) = req.defaultPriceScore` (default 0.5, neutral).

### 2.5 Default weights per objective

| objective | wL | wB | wT | wP | wD |
|---|---|---|---|---|---|
| `latency` | 0.50 | 0.15 | 0.20 | 0.05 | 0.10 |
| `throughput` | 0.10 | 0.50 | 0.25 | 0.05 | 0.10 |
| `balanced` | 0.25 | 0.25 | 0.25 | 0.10 | 0.15 |
| `cheap` | 0.10 | 0.20 | 0.20 | 0.40 | 0.10 |

Caller-supplied `req.weights` override these. Weights are normalized so wL+wB+wT+wP = 1 (wD is a
separate penalty coefficient applied during selection).

---

## 3. Redundancy factor — how many hosts

The number of hosts a job actually needs is **not** just `req.k`. Low-trust placement demands
redundancy (run identical work on independent hosts and compare — swarm's `verify`):

```
effectiveK = max( req.k,
                  redundancyFor(bestFeasibleTrust, req.redundancy) )
```

`req.redundancy` is a policy: `"none"` (trust the single best), `"verify"` (K-of independent hosts,
compare outputs), or a target confidence. `redundancyFor` maps the trust of the hosts we can reach
to a replication count: high-trust hosts ⇒ 1; medium ⇒ 2–3; low/unverified ⇒ 3+ on **independent**
operators (§4). For deterministic work, redundancy enables majority-vote verification; for
non-deterministic work it is plain replication for availability.

---

## 4. Vendor / risk model (diversity)

Do not pile a job onto one operator or one correlated cluster. Each candidate is tagged with
**grouping keys** that approximate independence:

| key | derived from | meaning |
|---|---|---|
| `operator` | node id → on-chain identity / owner (best available proxy: the node id itself) | who controls the host |
| `asn` | (future) profile/runtime hint or netgraph metadata | network provider — correlated outage/eclipse risk |
| `region` | `graph.regions()` membership | latency cluster — also a correlation proxy (same DC/LAN) |
| `cluster` | union of {operator, asn, region} via union-find | the broadest correlation bucket |

Two **concentration caps** bound how much of one job (and one user's recent jobs) lands on a single
group:

```
perGroupCap(group) = ceil( effectiveK · req.maxShare )      // default maxShare = 0.34 → no group > ~1/3
```

- **Within a job:** during selection, a candidate is rejected if adding it would exceed
  `perGroupCap` for any of its groups (operator, asn, region, cluster). This guarantees a `k≥3` job
  spans at least `ceil(1/maxShare)` independent operators.
- **Across a user's jobs:** the caller may pass `req.recentPlacements` (recent operator → count for
  this payer). The diversity penalty (§2, `wD`) grows with how concentrated this payer's recent work
  already is on the candidate's operator — spreading a *user's* exposure over time, not just one job.

```
diversityPenalty(c | chosen) =
      groupLoad(c, chosen)                     // fraction of `chosen` already in c's groups, [0,1]
    + recentLoad(c, req.recentPlacements)      // this payer's recent concentration on c's operator
```

The penalty is what makes the second-best-but-different host beat the second-best-but-same host.
**Redundancy hosts MUST be on independent operators** — for `redundancy = "verify"`, the placer
enforces a hard "distinct operator per replica" constraint on top of the soft penalty, because
verification across two hosts of the same operator proves nothing.

---

## 5. Constraint-satisfaction selection (the placer)

Greedy, constraint-aware, deterministic given a beacon seed:

```
1. pool        = candidates passing §1 hard filter
2. effectiveK  = §3
3. pre-score every candidate (§2.1–2.4) → static score
4. chosen = []
5. while len(chosen) < effectiveK and pool not empty:
     a. for each c in pool, live = staticScore(c) - wD·diversityPenalty(c | chosen)
     b. drop any c whose selection would break a HARD cap:
          - perGroupCap exceeded for any group of c, OR
          - redundancy="verify" and c.operator already in chosen's operators
     c. if no c survives (b), relax the SOFT region cap first, then asn, then operator
        (record `relaxed` in the plan so the caller sees independence was compromised);
        if even operator cannot be satisfied, stop and return a SHORT plan with `shortfall`.
     d. pick the max-`live` candidate; BEACON TIE-BREAK (§6) among near-ties (within `req.tieEps`).
     e. move it from pool → chosen.
6. cohort co-location pass (§7) for multi-stage DAG jobs.
7. emit PlacementPlan.
```

The selector is greedy, not globally optimal, by design: placement runs hot and often, the pool is
modest, and the diversity caps + redundancy are the constraints that actually matter for safety —
greedy with live re-ranking honors all of them. A swap-improvement local-search pass over the chosen
set is an optional refinement (off by default; `req.refine`).

---

## 6. Beacon-seeded selection (anti-collusion, BFT-safe)

Per compute-fabric.md §9, a host must not be able to predict or steer whether it is chosen.

- The placer derives a deterministic PRNG seed from `GET /beacon` (`height` + `hash`) **mixed with
  the request identity** (a job nonce / payer id) so the seed is unpredictable *before dispatch* yet
  fully auditable *after*: anyone can replay `(beacon, request, candidate set) → same plan`.
- The seed is used in two places:
  1. **Tie-break** (default): among candidates within `req.tieEps` of the top `live` score, the
     beacon PRNG picks the winner. This removes the deterministic "always the same host wins" bias
     without sacrificing quality — only genuine near-ties are randomized.
  2. **Stochastic selection** (`req.selection = "weighted"`): pick with probability proportional to
     `live` score (softmax with `req.temperature`), seeded by the beacon. Stronger anti-steering for
     low-stakes / high-redundancy jobs; the scorer still gates the candidate set so quality holds.
- For high-stakes selection use a **confirmed-depth** beacon block (the tip can reorg), exposed as
  `req.beaconDepth`. The plan records the exact `{height, hash}` used so the choice is verifiable.

Note the api.md caveat: a beacon-seeded pick is *verifiable* but *predictable once the beacon is
known*. For redundancy/anti-collusion where unpredictability-at-dispatch matters more than
auditability, set `req.selection = "weighted"` with a fresh per-dispatch nonce mixed into the seed.

---

## 7. Cohort co-location (multi-node / DAG jobs)

For `k > 1` the relationship *between* chosen hosts matters and depends on the job shape
(`req.cohort`):

- **`"spread"`** (default for redundancy/verify): maximize independence — the diversity penalty and
  caps already do this; additionally prefer hosts in *different* `regions()` so a region-wide outage
  cannot take all replicas.
- **`"colocate"`** (chatty / tensor-parallel / all-reduce): prefer hosts in the **same latency
  region** (`graph.regions()`), minimizing inter-host predicted RTT — the communication cost term.
  The placer scores cohort members additionally on mean predicted RTT to already-chosen members.
- **`"dag"`** (pipeline stages with data dependencies): given `req.dag` (stage adjacency + per-edge
  data volume), place adjacent stages on low-RTT edges (chatty stages graph-adjacent) and independent
  stages spread — minimizing makespan + communication cost (compute-fabric.md §5). v0 ships
  `spread`/`colocate`; `dag` is a documented extension point with the same selector core (the DAG
  adds inter-stage RTT to the live score).

These two pressures conflict — co-location reduces latency but concentrates risk. The cohort mode is
how the caller declares which one wins for this job. `colocate` relaxes the region diversity cap (but
never the operator cap — co-located replicas on one operator still prove nothing for verification).

---

## 8. Output: PlacementPlan

The plan is **advice + provenance**, not an action. The caller dispatches it with `ce.meshDeploy`
(or hands it to swarm). It records every input that determined the choice so the placement is
auditable and replayable:

```
PlacementPlan {
  targets:    [ { nodeId, score, rttMs, benchFit, trust, groups, replica } ],
  effectiveK, requestedK,
  beacon:     { height, hash },          // exact randomness used
  weights, objective,                    // resolved knobs
  relaxed:    [ "region" | "asn" | ... ],// independence relaxations, if any
  shortfall:  number,                    // missing replicas if the pool was too small/constrained
  rejected:   [ { nodeId, reason } ],    // why feasible-but-unchosen hosts lost (debuggability)
  assembledAtMs
}
```

`rejected` reasons (`stale`, `insufficient_mem`, `missing_tag`, `unreachable`, `group_cap`,
`operator_dup`, `low_score`, `benchmark_suspect`) make placement debuggable — the same
debugging-first stance as the agent framework's `doctor`/`trace`.

---

## Module contracts (so four implementers work in parallel)

Each module below is independently implementable against these frozen signatures. Shared shapes live
in `types.js`; the data client lives in `ce.js`. Implementers must not change these signatures
without updating this section. All money is base-unit **strings**/`BigInt`, never floats.

### `src/graph.js` — latency queries (standalone port of @ce-net/graph concepts)

A dependency-free port of the `@ce-net/graph` query surface, built from `/netgraph` observations
(+ optional `/atlas` capacity). Same algorithms as `ce-ts/graph/src/` (Vivaldi/MDS embedding,
union-find regions, Dijkstra) but plain `.js`. Do NOT import from `ce-ts`.

```js
// build from raw CE responses (see ce.js return shapes)
export function buildGraph(netgraphByOrigin, atlas, options) -> Graph
//   netgraphByOrigin: Map<originNodeId, RawNetGraphEdge[]>  (origin = the node /netgraph was fetched from)
//   atlas:            RawAtlasEntry[]
//   options:          { regionThresholdMs?, embedding?: { dimensions?, iterations?, seed? } }

class Graph {
  nodes()                      -> NodeId[]
  has(node)                    -> boolean
  measuredRtt(a, b)            -> number | undefined     // ground-truth direct sample
  predictedRtt(a, b)           -> number                 // embedding distance; shortest-path fallback; Infinity if unreachable
  kNearest(node, k)            -> NodeId[]                // ascending predicted RTT
  regions()                    -> NodeId[][]              // components under regionThresholdMs
  shortestPath(a, b)           -> NodeId[]                // min-RTT path; [] if unreachable
  regionOf(node)               -> number                 // region index (stable per build) — ce-sched extension
  coordinate(node)             -> Coordinate | undefined
  capacityOf(node)             -> NodeCapacity | undefined
  snapshot()                   -> FabricSnapshot
}
```

`regionOf` is the only addition beyond the @ce-net/graph contract — vendor.js needs an O(1) region
key. It is `regions()` flattened to an index map, computed once at build.

### `src/scorer.js` — per-candidate scalar score

Pure functions, no I/O. Consumes a `Candidate` (already feasibility-filtered, §1) and the resolved
request.

```js
export function resolveWeights(req)                 -> { wL, wB, wT, wP, wD }   // §2.5 + normalization
export function latencyScore(candidate, req, graph) -> number  // [0,1], §2.1
export function benchFitScore(candidate, req)       -> { score, source, confidence } // §2.2
export function trustScore(candidate, req)          -> { score, benchmarkSuspect }   // §2.3 (+ §9 cross-check)
export function priceScore(candidate, req)          -> number  // [0,1], §2.4  (BigInt math, ratio-only float)
// static score = the convex blend of the four terms above (NO diversity penalty — that is contextual)
export function staticScore(candidate, req, graph)  -> { score, parts: { latency, benchFit, trust, price } }
```

`staticScore` is the single entry the placer calls per candidate; the granular fns are exported for
testing and for the explorer to display the breakdown.

### `src/vendor.js` — diversity / risk

Pure functions over candidates + chosen set. No I/O.

```js
export function groupKeys(candidate, graph)            -> { operator, asn, region, cluster }  // §4
export function tagCandidates(candidates, graph)       -> Candidate[]   // attach .groups in place / returns new
export function perGroupCap(effectiveK, maxShare)      -> number        // §4 ceil(k·maxShare)
export function diversityPenalty(candidate, chosen, req) -> number      // [0,~2], §4
export function violatesCap(candidate, chosen, caps)   -> string | null // returns offending group key or null
export function clusterOf(candidates)                  -> Map<NodeId,string> // union-find over operator/asn/region
```

`violatesCap` is the hard gate the placer calls in step 5b; `diversityPenalty` is the soft term in
step 5a. `redundancy="verify"` operator-dup is enforced by the placer using `groupKeys().operator`.

### `src/placer.js` — the selector

Orchestrates everything. The only module that calls `ce.js` (or accepts pre-fetched data for
testing).

```js
// high-level: fetch everything, build graph, select, return a plan
export async function plan(req, ce, options) -> PlacementPlan
//   req: PlacementRequest, ce: the CE client (src/ce.js), options: { now?, beaconDepth? }

// pure core (no I/O) — for deterministic tests and reuse:
export function feasible(candidatesRaw, req, graph, now) -> Candidate[]              // §1 hard filter → Candidate[]
export function redundancyFor(candidates, req)           -> number                  // §3 effectiveK
export function select(candidates, req, graph, seed)     -> { targets, relaxed, shortfall, rejected } // §5–§7
export function beaconSeed(beacon, req)                  -> number                  // §6 deterministic PRNG seed
```

`plan` = gather (ce.netgraph/atlas/history/beacon) → buildGraph → feasible → scorer (via select) →
select → assemble `PlacementPlan`. `select` is pure given a `seed`, so the whole policy is unit-
testable with fixture data and a fixed beacon — no live node needed.
