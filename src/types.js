/**
 * Shared shapes for @ce-net/sched.
 *
 * Vanilla JS has no static types, so these are documented with JSDoc `@typedef`s. They are the
 * single source of truth that scorer.js / vendor.js / placer.js / graph.js all agree on. The first
 * block shadows the @ce-net/graph wire+domain types (we deliberately do NOT import from ce-ts);
 * the second block is ce-sched's own placement vocabulary.
 *
 * Money is always integer base units carried as decimal STRINGS (and parsed to BigInt). The only
 * floats anywhere are normalized [0,1] ranking scores — never an amount.
 *
 * This module exports no runtime values except tiny helpers; it exists for the typedefs and the
 * defaults table. Importing it is cheap.
 *
 * @module types
 */

// ----------------------------------------------------------------------------
// Shadowed @ce-net/graph types (wire + domain). Mirror of ce-ts/graph/src/types.ts.
// ----------------------------------------------------------------------------

/**
 * @typedef {string} NodeId  A libp2p PeerId / CE node id (hex), used as the stable graph key.
 */

/**
 * A single raw `/netgraph` entry exactly as the node serializes it (snake_case).
 * @typedef {object} RawNetGraphEdge
 * @property {string} peer            Hex PeerId of the directly connected peer.
 * @property {number} rtt_ms          EWMA round-trip time to that peer (ms).
 * @property {number} samples         Ping samples folded into the estimate.
 * @property {number} last_seen_secs  Unix seconds of the most recent sample.
 */

/**
 * A single raw `/atlas` entry (snake_case). `profile` is the future signed NodeProfile
 * (see docs/node-profile-spec.md); absent until P2 ships.
 * @typedef {object} RawAtlasEntry
 * @property {string}   node_id
 * @property {number}   cpu_cores
 * @property {number}   mem_mb
 * @property {number}   running_jobs
 * @property {number}   last_seen_secs
 * @property {string[]} [tags]
 * @property {NodeProfile} [profile]
 */

/**
 * A directed measured observation: `origin` measured `rttMs` to `peer`. The graph fuses the two
 * directions of a pair into one undirected {@link Edge}.
 * @typedef {object} MeasuredObservation
 * @property {NodeId} origin
 * @property {NodeId} peer
 * @property {number} rttMs
 * @property {number} samples
 * @property {number} lastSeenSecs
 */

/**
 * An undirected, sample-weighted-fused edge in the assembled graph.
 * @typedef {object} Edge
 * @property {NodeId} a
 * @property {NodeId} b
 * @property {number} rttMs
 * @property {number} samples
 * @property {number} lastSeenSecs
 */

/**
 * Live capacity for a node, normalized to camelCase.
 * @typedef {object} NodeCapacity
 * @property {NodeId}   nodeId
 * @property {number}   cpuCores
 * @property {number}   memMb
 * @property {number}   runningJobs
 * @property {number}   lastSeenSecs
 * @property {string[]} tags
 */

/**
 * A network-coordinate (Vivaldi/MDS embedding position).
 * @typedef {object} Coordinate
 * @property {NodeId}   nodeId
 * @property {number[]} vec
 */

/**
 * Serializable snapshot of the assembled graph (the `snapshot()` of the query contract).
 * @typedef {object} FabricSnapshot
 * @property {NodeId[]}      nodes
 * @property {Edge[]}        edges
 * @property {Coordinate[]}  coordinates
 * @property {NodeCapacity[]} capacity
 * @property {number}        assembledAtMs
 */

// ----------------------------------------------------------------------------
// Signed NodeProfile (consumer view). See docs/node-profile-spec.md.
// ----------------------------------------------------------------------------

/**
 * @typedef {object} CpuProfile
 * @property {number} cores
 * @property {number} threads
 * @property {number} gflops_fp32
 * @property {number} mem_bw_gbps
 *
 * @typedef {object} GpuProfile
 * @property {string} model
 * @property {"Cuda"|"Metal"|"Rocm"|"Vulkan"} backend
 * @property {number} vram_mb
 * @property {number} fp16_tflops
 *
 * @typedef {object} NodeProfile
 * @property {NodeId}     node_id
 * @property {number}     measured_at
 * @property {CpuProfile} cpu
 * @property {GpuProfile[]} gpus
 * @property {{ total_mb:number, available_mb:number }} memory
 * @property {{ total_gb:number, free_gb:number, read_mbps:number, write_mbps:number }} storage
 * @property {{ ref_model:string, tokens_per_sec:number }} llm
 * @property {{ os:string, arch:string, docker:boolean, gvisor:boolean, wasm:boolean }} runtime
 * @property {string}     [sig]
 */

// ----------------------------------------------------------------------------
// ce-sched placement vocabulary.
// ----------------------------------------------------------------------------

/**
 * The demand vector: which benchmark axes matter for this job and how much, plus the "enough" point
 * per axis. `weight` is relative (normalized internally); `target` is the saturating bar (units of
 * the axis). Omitted axes have demand 0.
 * @typedef {object} DemandAxis
 * @property {number} weight
 * @property {number} target
 *
 * @typedef {object} Demand
 * @property {DemandAxis} [gflops]        cpu.gflops_fp32
 * @property {DemandAxis} [memBwGbps]     cpu.mem_bw_gbps
 * @property {DemandAxis} [vramMb]        max over gpus[].vram_mb
 * @property {DemandAxis} [fp16Tflops]    sum over gpus[].fp16_tflops
 * @property {DemandAxis} [tokensPerSec]  llm.tokens_per_sec
 * @property {DemandAxis} [diskReadMbps]  storage.read_mbps
 * @property {DemandAxis} [diskWriteMbps] storage.write_mbps
 */

/**
 * Blend weights (latency, benchFit, trust, price, diversity). wL+wB+wT+wP normalize to 1; wD is a
 * separate penalty coefficient applied during selection. See placement-design.md §2.5.
 * @typedef {object} Weights
 * @property {number} wL
 * @property {number} wB
 * @property {number} wT
 * @property {number} wP
 * @property {number} wD
 */

/**
 * Everything the caller declares about a job + how to place it. All money is a base-unit string.
 * @typedef {object} PlacementRequest
 * @property {NodeId}   payer                 latency origin (the node whose /netgraph anchors "near me")
 * @property {number}   k                     requested host count (effectiveK may be larger via redundancy)
 * @property {number}   cpuCores              hard requirement per host
 * @property {number}   memMb                 hard requirement per host
 * @property {string[]} [requireTags]         hard tag requirements (e.g. ["docker","gpu"])
 * @property {NodeId[]} [exclude]             node ids to never pick
 * @property {boolean}  [allowSelf=false]     allow placing on the payer node
 * @property {number}   [maxStaleSecs=180]    atlas liveness window
 * @property {"latency"|"throughput"|"balanced"|"cheap"} [objective="balanced"]
 * @property {Weights}  [weights]             overrides the objective's defaults
 * @property {Demand}   [demand]              benchmark demand vector (§2.2)
 * @property {number}   [minGflops]           capability floor (hard if requireProfile)
 * @property {number}   [minVramMb]
 * @property {number}   [minTokensPerSec]
 * @property {boolean}  [requireProfile=false] exclude hosts without a measured profile when a floor is set
 * @property {number}   [rttSoftCapMs=250]    latency normalization cap (§2.1)
 * @property {number}   [trustSaturation=50]  delivered-work count at which trust ~saturates (§2.3)
 * @property {string}   [priceCapBaseUnits]   base-unit string; price normalization cap (§2.4)
 * @property {number}   [defaultPriceScore=0.5] price score when a host advertises no ask
 * @property {"none"|"verify"|number} [redundancy="none"] redundancy policy / target confidence (§3)
 * @property {number}   [maxShare=0.34]       max fraction of effectiveK on any one group (§4)
 * @property {Object<string,number>} [recentPlacements] this payer's recent operator -> count (§4)
 * @property {"spread"|"colocate"|"dag"} [cohort="spread"]  multi-host relationship (§7)
 * @property {object}   [dag]                 stage adjacency + data volumes (cohort="dag", §7)
 * @property {"best"|"weighted"} [selection="best"]  beacon tie-break vs stochastic (§6)
 * @property {number}   [temperature=0.15]    softmax temperature for selection="weighted"
 * @property {number}   [tieEps=0.02]         score window treated as a tie for the beacon break (§6)
 * @property {number}   [beaconDepth=0]       confirmed-depth offset for high-stakes selection (§6)
 * @property {string}   [nonce]               per-request entropy mixed into the beacon seed (§6)
 * @property {boolean}  [refine=false]        run the optional swap-improvement pass (§5)
 */

/**
 * A feasibility-filtered, scored placement candidate. Built by placer.feasible, enriched by the
 * scorer and vendor modules.
 * @typedef {object} Candidate
 * @property {NodeId}        nodeId
 * @property {NodeCapacity}  capacity
 * @property {NodeProfile|null} profile        null => atlas fallback in use
 * @property {object|null}   history           raw /history stats (or null if not fetched)
 * @property {number}        rttMs             measured-or-predicted RTT to payer
 * @property {boolean}       rttMeasured       true if rttMs is a direct measured sample
 * @property {{operator:string, asn:string, region:string, cluster:string}} [groups]  set by vendor.js
 * @property {string|undefined} askBaseUnits   advertised price (base-unit string), if any
 * @property {object}        [score]           set by scorer/placer: { total, parts:{...}, benchFit:{source,confidence}, benchmarkSuspect }
 * @property {number}        [replica]         set by placer: replica index within the cohort
 */

/**
 * A chosen target row in the plan.
 * @typedef {object} PlanTarget
 * @property {NodeId} nodeId
 * @property {number} score
 * @property {number} rttMs
 * @property {number} benchFit
 * @property {number} trust
 * @property {{operator:string, asn:string, region:string, cluster:string}} groups
 * @property {number} replica
 */

/**
 * The placement decision: advice + provenance, not an action. See placement-design.md §8.
 * @typedef {object} PlacementPlan
 * @property {PlanTarget[]} targets
 * @property {number}       effectiveK
 * @property {number}       requestedK
 * @property {{height:number, hash:string}} beacon
 * @property {Weights}      weights
 * @property {string}       objective
 * @property {string[]}     relaxed          independence relaxations applied, if any
 * @property {number}       shortfall        missing replicas (0 if fully satisfied)
 * @property {{nodeId:NodeId, reason:string}[]} rejected
 * @property {number}       assembledAtMs
 */

/** Default blend weights per objective (placement-design.md §2.5). wD is the penalty coefficient. */
export const OBJECTIVE_WEIGHTS = Object.freeze({
  latency: { wL: 0.5, wB: 0.15, wT: 0.2, wP: 0.05, wD: 0.1 },
  throughput: { wL: 0.1, wB: 0.5, wT: 0.25, wP: 0.05, wD: 0.1 },
  balanced: { wL: 0.25, wB: 0.25, wT: 0.25, wP: 0.1, wD: 0.15 },
  cheap: { wL: 0.1, wB: 0.2, wT: 0.2, wP: 0.4, wD: 0.1 },
});

/** Defaults for every optional PlacementRequest field, in one place. */
export const REQUEST_DEFAULTS = Object.freeze({
  allowSelf: false,
  maxStaleSecs: 180,
  objective: "balanced",
  requireProfile: false,
  rttSoftCapMs: 250,
  trustSaturation: 50,
  defaultPriceScore: 0.5,
  redundancy: "none",
  maxShare: 0.34,
  cohort: "spread",
  selection: "best",
  temperature: 0.15,
  tieEps: 0.02,
  beaconDepth: 0,
  refine: false,
});

/** Clamp a number to [0,1]. Shared by the scorer modules. */
export function clamp01(x) {
  if (Number.isNaN(x)) return 0;
  if (x < 0) return 0;
  if (x > 1) return 1;
  return x;
}

/**
 * Resolve a PlacementRequest against REQUEST_DEFAULTS without mutating the input. Returns a new
 * object every caller can read fully-populated optional fields from.
 * @param {PlacementRequest} req
 * @returns {PlacementRequest}
 */
export function withDefaults(req) {
  return { ...REQUEST_DEFAULTS, ...req };
}
