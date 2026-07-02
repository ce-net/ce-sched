/**
 * @ce-net/sched-sdk — typed TypeScript/JavaScript client for the ce-sched per-machine daemon.
 *
 * Talks to the local ce-sched daemon's HTTP API (default `http://127.0.0.1:8856`), the JS/TS
 * counterpart to the Rust `ce-sched-sdk`. The wire types mirror `ce-sched-core::api` exactly
 * (camelCase placement vocabulary; snake_case FabricMap, matching ce-bench).
 *
 * Money is always integer base units carried as DECIMAL STRINGS (they exceed 2^53) — never coerce a
 * `*BaseUnits` field to `number`.
 *
 * @example
 * import { SchedClient } from "@ce-net/sched-sdk";
 * const sched = new SchedClient();                       // http://127.0.0.1:8856
 * const plan = await sched.plan({ cpuCores: 1, memMb: 256, requireTags: ["docker"] });
 * for (const t of plan.targets) console.log(t.nodeId, t.score);
 *
 * @module index
 */

// ----------------------------------------------------------------------------
// Wire types (mirror of ce-sched-core::api).
// ----------------------------------------------------------------------------

/** A libp2p PeerId / CE node id (hex). */
export type NodeId = string;

export type Objective = "latency" | "throughput" | "balanced" | "cheap";
export type Cohort = "spread" | "colocate" | "dag";
export type Selection = "best" | "weighted";
/** `"none" | "verify" | <number in (0,1)>` (target confidence). */
export type Redundancy = "none" | "verify" | number;

/** Blend weights. wL+wB+wT+wP normalize to 1; wD is a separate penalty coefficient. */
export interface Weights {
  wL: number;
  wB: number;
  wT: number;
  wP: number;
  wD: number;
}

/** One demand axis: relative `weight` + the saturating `target` ("enough") bar. */
export interface DemandAxis {
  weight: number;
  target: number;
}

/** Benchmark demand vector (omitted axes have demand 0). */
export interface Demand {
  gflops?: DemandAxis;
  memBwGbps?: DemandAxis;
  vramMb?: DemandAxis;
  fp16Tflops?: DemandAxis;
  tokensPerSec?: DemandAxis;
  diskReadMbps?: DemandAxis;
  diskWriteMbps?: DemandAxis;
}

/** The `/plan` request body. Only `cpuCores` + `memMb` are required; the daemon fills the rest. */
export interface JobSpec {
  payer?: NodeId;
  k?: number;
  cpuCores: number;
  memMb: number;
  requireTags?: string[];
  exclude?: NodeId[];
  allowSelf?: boolean;
  maxStaleSecs?: number;
  objective?: Objective;
  weights?: Weights;
  demand?: Demand;
  minGflops?: number;
  minVramMb?: number;
  minTokensPerSec?: number;
  requireProfile?: boolean;
  rttSoftCapMs?: number;
  trustSaturation?: number;
  /** base-unit decimal string */
  priceCapBaseUnits?: string;
  defaultPriceScore?: number;
  redundancy?: Redundancy;
  maxShare?: number;
  recentPlacements?: Record<string, number>;
  cohort?: Cohort;
  selection?: Selection;
  temperature?: number;
  tieEps?: number;
  beaconDepth?: number;
  nonce?: string;
  refine?: boolean;
}

/** Vendor / correlation grouping keys for a chosen host. */
export interface Groups {
  operator: string;
  asn: string;
  region: string;
  cluster: string;
}

/** A chosen target row in the plan. */
export interface PlanTarget {
  nodeId: NodeId;
  score: number;
  rttMs: number;
  benchFit: number;
  trust: number;
  groups: Groups;
  replica: number;
}

export interface BeaconRef {
  height: number;
  hash: string;
}

export interface RejectedHost {
  nodeId: NodeId;
  reason: string;
}

/** The `/plan` response body. */
export interface PlacementPlan {
  targets: PlanTarget[];
  effectiveK: number;
  requestedK: number;
  beacon: BeaconRef;
  weights: Weights;
  objective: string;
  relaxed: string[];
  shortfall: number;
  rejected: RejectedHost[];
  assembledAtMs: number;
}

/** What to deploy on each chosen target (mirrors ce-rs BidSpec + an optional grant). */
export interface DeploySpec {
  image: string;
  cmd?: string[];
  cpuCores: number;
  memMb: number;
  durationSecs: number;
  /** base-unit decimal string */
  bid: string;
  grant?: string;
}

/** The `/place` request body. */
export interface PlaceRequest {
  plan: PlacementPlan;
  deploy: DeploySpec;
}

export interface Dispatched {
  nodeId: NodeId;
  replica: number;
  ok: boolean;
  jobId?: string;
  error?: string;
}

/** The `/place` response body. */
export interface DispatchResult {
  dispatched: Dispatched[];
  placed: number;
  assembledAtMs: number;
}

// --- FabricMap family (snake_case wire, mirrors ce-bench) ---------------------

export interface NodeCapacity {
  node_id: NodeId;
  cpu_cores: number;
  mem_mb: number;
  running_jobs: number;
  last_seen_secs: number;
  tags: string[];
  ask_base_units?: string;
}

export interface NodeProfile {
  node_id: NodeId;
  measured_at: number;
  cpu: { cores: number; threads: number; gflops_fp32: number; mem_bw_gbps: number };
  gpus: Array<{ model: string; backend: string; vram_mb: number; fp16_tflops: number }>;
  memory: { total_mb: number; available_mb: number };
  storage: { total_gb: number; free_gb: number; read_mbps: number; write_mbps: number };
  llm: { ref_model: string; tokens_per_sec: number };
  runtime: { os: string; arch: string; docker: boolean; gvisor: boolean; wasm: boolean; kind?: string };
  sig?: string;
}

/**
 * One node's on-chain interaction facts (mirror of the node's `GET /history/:id`) — the reputation
 * substrate the trust score reads. Money fields are base-unit decimal STRINGS.
 */
export interface NodeHistory {
  node_id: NodeId;
  jobs_hosted: number;
  heartbeats_hosted: number;
  earned: string;
  recent_earned: string;
  owner?: string;
  operator?: string;
}

export interface FabricNode {
  node_id: NodeId;
  capacity: NodeCapacity;
  profile?: NodeProfile;
  /** On-chain interaction facts (the trust substrate); absent when the assembler didn't fetch them. */
  history?: NodeHistory;
}

export interface MeshEdge {
  a: NodeId;
  b: NodeId;
  rtt_ms: number;
  samples: number;
  last_seen_secs: number;
}

export interface FabricStats {
  nodes: number;
  cpu_cores: number;
  cpu_gflops: number;
  gpus: number;
  gpu_vram_mb: number;
  gpu_tflops: number;
  tokens_per_sec: number;
  storage_free_gb: number;
  perf_score: number;
  mesh: { median_rtt_ms: number; reachable_frac: number; regions: number };
  by_kind: { native: number; container: number; browser: number };
  computed_at: number;
}

/** The assembled Fabric Map served by `GET /map`. */
export interface FabricMap {
  nodes: FabricNode[];
  edges: MeshEdge[];
  origin?: NodeId;
  stats: FabricStats;
  /** Public-randomness reference captured at assembly time; seeds the §6 tie-break. */
  beacon: BeaconRef;
  assembled_at_ms: number;
}

// ----------------------------------------------------------------------------
// Client
// ----------------------------------------------------------------------------

/** Default local ce-sched daemon HTTP API base URL. */
export const DEFAULT_BASE_URL = "http://127.0.0.1:8856";

export interface SchedClientOptions {
  fetch?: typeof fetch;
  timeoutMs?: number;
  headers?: Record<string, string>;
}

/** Typed client for the per-machine ce-sched daemon. */
export class SchedClient {
  readonly baseUrl: string;
  private readonly _fetch: typeof fetch;
  private readonly timeoutMs: number;
  private readonly headers: Record<string, string>;

  /**
   * @param baseUrl daemon base URL, default `http://127.0.0.1:8856`
   */
  constructor(baseUrl: string = DEFAULT_BASE_URL, options: SchedClientOptions = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    const f = options.fetch ?? globalThis.fetch;
    if (typeof f !== "function") {
      throw new Error("SchedClient: no fetch available; pass options.fetch on this runtime");
    }
    this._fetch = f;
    this.timeoutMs = options.timeoutMs ?? 8000;
    this.headers = options.headers ?? {};
  }

  /** Internal: fetch a path and decode JSON, with a timeout + a useful error on non-2xx. */
  private async _req<T>(path: string, init: RequestInit = {}): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const res = await this._fetch(`${this.baseUrl}${path}`, {
        ...init,
        signal: controller.signal,
        headers: {
          accept: "application/json",
          ...(init.body ? { "content-type": "application/json" } : {}),
          ...this.headers,
          ...(init.headers ?? {}),
        },
      });
      const text = await res.text();
      if (!res.ok) {
        throw new Error(`ce-sched ${init.method ?? "GET"} ${path} -> ${res.status}: ${text.slice(0, 300)}`);
      }
      return (text ? JSON.parse(text) : undefined) as T;
    } finally {
      clearTimeout(timer);
    }
  }

  /** `GET /health` — true if the daemon is up. */
  async health(): Promise<boolean> {
    try {
      await this._req<unknown>("/health");
      return true;
    } catch {
      return false;
    }
  }

  /** `GET /map` — the assembled Fabric Map the daemon plans against. */
  async map(): Promise<FabricMap> {
    return this._req<FabricMap>("/map");
  }

  /** `POST /plan` — submit a JobSpec, get a PlacementPlan. */
  async plan(spec: JobSpec): Promise<PlacementPlan> {
    return this._req<PlacementPlan>("/plan", { method: "POST", body: JSON.stringify(spec) });
  }

  /** `POST /place` — dispatch a plan's targets via the node's /mesh-deploy. */
  async place(req: PlaceRequest): Promise<DispatchResult> {
    return this._req<DispatchResult>("/place", { method: "POST", body: JSON.stringify(req) });
  }
}
