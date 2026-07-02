//! Shared daemon-API wire types for ce-sched.
//!
//! These are the typed contract that the daemon (`ce-sched-daemon`), the Rust SDK
//! (`ce-sched-sdk`), and the TS SDK (`sdk-ts`) all agree on. They are the Rust port of the JSDoc
//! `@typedef`s in the JS `src/types.js`, plus the `FabricMap` shape the daemon serves.
//!
//! The daemon surface (see `docs/api-spec.md`):
//!
//! | Method | Path     | Request            | Response         |
//! |--------|----------|--------------------|------------------|
//! | POST   | `/plan`  | [`JobSpec`]        | [`PlacementPlan`]|
//! | POST   | `/place` | [`PlaceRequest`]   | [`DispatchResult`]|
//! | GET    | `/map`   | —                  | [`FabricMap`]    |
//! | GET    | `/health`| —                  | `{ "status": .. }`|
//!
//! ## Money
//! CE amounts are integer base units carried as **decimal strings** (`1 credit = 10^18` base units),
//! because they exceed JSON's `2^53`. Every money field here is a `String`; the scorer parses it to a
//! 128-bit integer where it needs to compare. The only floats anywhere are normalized `[0,1]` ranking
//! scores — never an amount.
//!
//! ## Wire casing
//! The placement vocabulary ([`JobSpec`], [`PlacementPlan`], [`PlanTarget`], [`Weights`], …) is
//! serialized **camelCase** to match the existing JS/TS SDK wire (`cpuCores`, `requireTags`, `wL`, …).
//! The [`FabricMap`] family mirrors **ce-bench's** `FabricStats` / `NodeProfile` shapes, which are
//! snake_case on the wire, so those structs keep snake_case field names.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A libp2p PeerId / CE node id (hex), used as the stable graph key.
pub type NodeId = String;

// ============================================================================
// Placement vocabulary — the /plan request + response (camelCase wire).
// ============================================================================

/// Optimization objective; picks the default blend [`Weights`] (see [`OBJECTIVE_WEIGHTS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Objective {
    Latency,
    Throughput,
    Balanced,
    Cheap,
}

impl Default for Objective {
    fn default() -> Self {
        Objective::Balanced
    }
}

impl Objective {
    /// The wire string for this objective (matches the JS `OBJECTIVE_WEIGHTS` keys).
    pub fn as_str(&self) -> &'static str {
        match self {
            Objective::Latency => "latency",
            Objective::Throughput => "throughput",
            Objective::Balanced => "balanced",
            Objective::Cheap => "cheap",
        }
    }
}

/// Multi-host relationship for the cohort (§7 of placement-design.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cohort {
    /// Reward landing in a not-yet-used latency region (availability under a regional outage).
    Spread,
    /// Reward low predicted RTT to already-chosen members (chatty / tensor-parallel jobs).
    Colocate,
    /// Stage adjacency + per-edge data volumes; v0 treats it as `spread`.
    Dag,
}

impl Default for Cohort {
    fn default() -> Self {
        Cohort::Spread
    }
}

/// Target-picking strategy (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    /// Highest score, with a beacon-seeded tie-break among near-ties.
    Best,
    /// Softmax-sample from the live scores (beacon-seeded).
    Weighted,
}

impl Default for Selection {
    fn default() -> Self {
        Selection::Best
    }
}

/// Redundancy policy / target confidence (§3).
///
/// On the wire this is `"none" | "verify" | <number in (0,1)>` (a union the JS API uses). Modeled
/// here as an untagged enum: a JSON number deserializes to [`Redundancy::Confidence`], the strings to
/// [`Redundancy::None`] / [`Redundancy::Verify`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Redundancy {
    /// A target confidence in `(0,1)`: map the best feasible trust to the replica count needed.
    Confidence(f64),
    /// `"none"` (trust the single best) or `"verify"` (>= 2-3 independent, distinct-operator replicas).
    Policy(RedundancyPolicy),
}

/// The named redundancy policies (the string arm of [`Redundancy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedundancyPolicy {
    None,
    Verify,
}

impl Default for Redundancy {
    fn default() -> Self {
        Redundancy::Policy(RedundancyPolicy::None)
    }
}

/// One axis of the benchmark demand vector: how much an axis matters (`weight`, relative) and the
/// "enough" saturating bar (`target`, in units of the axis).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DemandAxis {
    pub weight: f64,
    pub target: f64,
}

/// The demand vector: which benchmark axes matter for this job. Omitted axes have demand 0.
/// Mirrors the seven `PROFILE_AXIS` keys of the JS scorer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Demand {
    /// `cpu.gflops_fp32`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gflops: Option<DemandAxis>,
    /// `cpu.mem_bw_gbps`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bw_gbps: Option<DemandAxis>,
    /// max over `gpus[].vram_mb`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_mb: Option<DemandAxis>,
    /// sum over `gpus[].fp16_tflops`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fp16_tflops: Option<DemandAxis>,
    /// `llm.tokens_per_sec`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<DemandAxis>,
    /// `storage.read_mbps`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_mbps: Option<DemandAxis>,
    /// `storage.write_mbps`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write_mbps: Option<DemandAxis>,
}

/// Blend weights. `wL+wB+wT+wP` normalize to 1 (see `scorer::resolve_weights`); `wD` is a separate
/// diversity-penalty coefficient applied during selection, carried verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Weights {
    /// latency weight
    pub w_l: f64,
    /// benchFit weight
    pub w_b: f64,
    /// trust weight
    pub w_t: f64,
    /// price weight
    pub w_p: f64,
    /// diversity penalty coefficient (NOT part of the convex normalization)
    pub w_d: f64,
}

/// The default blend weights per objective (placement-design.md §2.5). `wD` is the penalty coefficient.
pub const OBJECTIVE_WEIGHTS: [(Objective, Weights); 4] = [
    (Objective::Latency, Weights { w_l: 0.5, w_b: 0.15, w_t: 0.2, w_p: 0.05, w_d: 0.1 }),
    (Objective::Throughput, Weights { w_l: 0.1, w_b: 0.5, w_t: 0.25, w_p: 0.05, w_d: 0.1 }),
    (Objective::Balanced, Weights { w_l: 0.25, w_b: 0.25, w_t: 0.25, w_p: 0.1, w_d: 0.15 }),
    (Objective::Cheap, Weights { w_l: 0.1, w_b: 0.2, w_t: 0.2, w_p: 0.4, w_d: 0.1 }),
];

impl Weights {
    /// The default weights for an objective (looks up [`OBJECTIVE_WEIGHTS`]).
    pub fn for_objective(obj: Objective) -> Weights {
        OBJECTIVE_WEIGHTS
            .iter()
            .find(|(o, _)| *o == obj)
            .map(|(_, w)| *w)
            .unwrap_or(OBJECTIVE_WEIGHTS[2].1)
    }
}

/// Everything the caller declares about a job + how to place it. The `/plan` request body.
///
/// Port of the JS `PlacementRequest` typedef. Optional fields use `Option`/`#[serde(default)]` so a
/// minimal `{ "cpuCores": 1, "memMb": 256 }` is a valid request; the resolver fills the rest from
/// [`REQUEST_DEFAULTS`]-equivalent logic (see the `placer` module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    /// Latency origin (the node whose `/netgraph` anchors "near me"). The daemon auto-fills it from
    /// the local node's `/status` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<NodeId>,
    /// Requested host count (effectiveK may be larger via redundancy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<u32>,
    /// Hard CPU-core requirement per host.
    pub cpu_cores: u32,
    /// Hard memory (MB) requirement per host.
    pub mem_mb: u64,
    /// Hard tag requirements (e.g. `["docker","gpu"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_tags: Vec<String>,
    /// Node ids to never pick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<NodeId>,
    /// Allow placing on the payer node itself.
    #[serde(default)]
    pub allow_self: bool,
    /// Atlas liveness window (seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_stale_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<Objective>,
    /// Overrides the objective's default blend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Weights>,
    /// Benchmark demand vector (§2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand: Option<Demand>,
    /// Capability floor (hard when `requireProfile`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_gflops: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vram_mb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tokens_per_sec: Option<f64>,
    /// Exclude hosts without a measured profile when a floor is set.
    #[serde(default)]
    pub require_profile: bool,
    /// Latency normalization cap (§2.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_soft_cap_ms: Option<f64>,
    /// Delivered-work count at which trust ~saturates (§2.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_saturation: Option<f64>,
    /// Base-unit decimal string; price normalization cap (§2.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cap_base_units: Option<String>,
    /// Price score when a host advertises no ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_price_score: Option<f64>,
    /// Redundancy policy / target confidence (§3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<Redundancy>,
    /// Max fraction of effectiveK on any one group (§4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_share: Option<f64>,
    /// This payer's recent `operator -> count` (§4 across-job spread).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_placements: Option<BTreeMap<String, f64>>,
    /// Multi-host relationship (§7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohort: Option<Cohort>,
    /// Beacon tie-break vs stochastic selection (§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
    /// Softmax temperature for `selection = "weighted"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Score window treated as a tie for the beacon break (§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tie_eps: Option<f64>,
    /// Confirmed-depth offset for high-stakes selection (§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beacon_depth: Option<u64>,
    /// Per-request entropy mixed into the beacon seed (§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Run the optional swap-improvement pass (§5).
    #[serde(default)]
    pub refine: bool,
}

/// The vendor / correlation grouping keys for a chosen host (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Groups {
    pub operator: String,
    pub asn: String,
    pub region: String,
    pub cluster: String,
}

/// A chosen target row in the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTarget {
    pub node_id: NodeId,
    pub score: f64,
    pub rtt_ms: f64,
    pub bench_fit: f64,
    pub trust: f64,
    pub groups: Groups,
    pub replica: u32,
}

/// The verifiable public-randomness reference the plan was seeded from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BeaconRef {
    pub height: u64,
    pub hash: String,
}

/// A host that was feasible but not chosen, with the reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedHost {
    pub node_id: NodeId,
    pub reason: String,
}

/// The placement decision: advice + provenance, not an action. The `/plan` response body.
/// Port of the JS `PlacementPlan` typedef.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementPlan {
    pub targets: Vec<PlanTarget>,
    pub effective_k: u32,
    pub requested_k: u32,
    pub beacon: BeaconRef,
    pub weights: Weights,
    pub objective: String,
    /// Independence relaxations applied, if any (`region`/`asn`/`operator`).
    #[serde(default)]
    pub relaxed: Vec<String>,
    /// Missing replicas (0 if fully satisfied).
    pub shortfall: u32,
    #[serde(default)]
    pub rejected: Vec<RejectedHost>,
    pub assembled_at_ms: u64,
}

// ============================================================================
// /place — dispatch a plan via the node's /mesh-deploy.
// ============================================================================

/// The container/cell to deploy on each chosen target (mirrors `ce_rs::BidSpec` + an optional grant).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploySpec {
    pub image: String,
    #[serde(default)]
    pub cmd: Vec<String>,
    pub cpu_cores: u32,
    pub mem_mb: u64,
    pub duration_secs: u64,
    /// Funding committed per host, base-unit decimal string.
    pub bid: String,
    /// Optional capability grant token authorizing the deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

/// The `/place` request body: a plan plus what to deploy on its targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceRequest {
    pub plan: PlacementPlan,
    pub deploy: DeploySpec,
}

/// One host's dispatch outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispatched {
    pub node_id: NodeId,
    pub replica: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `/place` response body: per-target dispatch results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchResult {
    pub dispatched: Vec<Dispatched>,
    /// How many of `dispatched` succeeded.
    pub placed: u32,
    pub assembled_at_ms: u64,
}

// ============================================================================
// FabricMap — the assembled map the daemon plans against (GET /map).
//
// Mirrors ce-bench-core's FabricMap (snake_case wire): the folded per-node signed NodeProfiles, the
// mesh latency edges, and the FabricStats scoreboard. ce-sched consumes it from the local ce-bench
// daemon (:8855) and/or the node read-substrate; the planner reads only this, never the node directly.
// ============================================================================

/// CPU axis of a [`NodeProfile`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuProfile {
    pub cores: u32,
    #[serde(default)]
    pub threads: u32,
    #[serde(default)]
    pub gflops_fp32: f64,
    #[serde(default)]
    pub mem_bw_gbps: f64,
}

/// One GPU/accelerator in a [`NodeProfile`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub vram_mb: f64,
    #[serde(default)]
    pub fp16_tflops: f64,
}

/// Memory axis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    #[serde(default)]
    pub total_mb: f64,
    #[serde(default)]
    pub available_mb: f64,
}

/// Storage axis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageInfo {
    #[serde(default)]
    pub total_gb: f64,
    #[serde(default)]
    pub free_gb: f64,
    #[serde(default)]
    pub read_mbps: f64,
    #[serde(default)]
    pub write_mbps: f64,
}

/// LLM throughput axis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmInfo {
    #[serde(default)]
    pub ref_model: String,
    #[serde(default)]
    pub tokens_per_sec: f64,
}

/// Runtime / capability descriptor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub docker: bool,
    #[serde(default)]
    pub gvisor: bool,
    #[serde(default)]
    pub wasm: bool,
    /// `Native` | `Container` | `Browser` (browser nodes execute jobs slower; downranked by the scorer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The signed per-node capability vector (consumer view). Mirrors ce-bench's `NodeProfile`; the `sig`
/// is verified by ce-bench before it folds the profile into the map, so the planner trusts it as-is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeProfile {
    pub node_id: NodeId,
    #[serde(default)]
    pub measured_at: u64,
    #[serde(default)]
    pub cpu: CpuProfile,
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
    #[serde(default)]
    pub memory: MemoryInfo,
    #[serde(default)]
    pub storage: StorageInfo,
    #[serde(default)]
    pub llm: LlmInfo,
    #[serde(default)]
    pub runtime: RuntimeInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// Live capacity for a node, normalized from the node `/atlas` (snake->kept-snake on this wire).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCapacity {
    pub node_id: NodeId,
    #[serde(default)]
    pub cpu_cores: u32,
    #[serde(default)]
    pub mem_mb: u64,
    #[serde(default)]
    pub running_jobs: u32,
    #[serde(default)]
    pub last_seen_secs: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Advertised price (base-unit decimal string), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_base_units: Option<String>,
}

/// An undirected, sample-weighted-fused latency edge in the assembled map.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeshEdge {
    pub a: NodeId,
    pub b: NodeId,
    pub rtt_ms: f64,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub last_seen_secs: u64,
}

/// One node's on-chain interaction facts (mirror of the node's `GET /history/:id`, snake_case wire).
/// The reputation substrate the trust score reads. Money fields are base-unit decimal STRINGS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeHistory {
    #[serde(default)]
    pub node_id: NodeId,
    /// Jobs this node hosted and was paid for.
    #[serde(default)]
    pub jobs_hosted: u64,
    /// Heartbeats this node hosted and was paid for.
    #[serde(default)]
    pub heartbeats_hosted: u64,
    /// All-time earnings, base-unit decimal string.
    #[serde(default)]
    pub earned: String,
    /// Earnings within the node's recent accounting window, base-unit decimal string.
    #[serde(default)]
    pub recent_earned: String,
    /// On-chain owner record, when known — collapses multi-node operators into one vendor group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Alternate operator key some substrates report instead of `owner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
}

/// One node's row in the [`FabricMap`]: its capacity (always), its signed profile (when measured),
/// and its on-chain history (when the map assembler fetched it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FabricNode {
    pub node_id: NodeId,
    pub capacity: NodeCapacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<NodeProfile>,
    /// On-chain interaction facts (the trust substrate); `None` when not fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<NodeHistory>,
}

/// Network-wide scoreboard (mirror of ce-bench's `FabricStats`). No money fields — plain numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FabricStats {
    pub nodes: u64,
    pub cpu_cores: u64,
    pub cpu_gflops: f64,
    pub gpus: u64,
    pub gpu_vram_mb: f64,
    pub gpu_tflops: f64,
    pub tokens_per_sec: f64,
    pub storage_free_gb: f64,
    pub perf_score: f64,
    pub mesh: MeshSummary,
    pub by_kind: ByKind,
    pub computed_at: u64,
}

/// Mesh-quality summary inside [`FabricStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeshSummary {
    pub median_rtt_ms: f64,
    pub reachable_frac: f64,
    pub regions: u64,
}

/// Node-kind census inside [`FabricStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ByKind {
    pub native: u64,
    pub container: u64,
    pub browser: u64,
}

/// The assembled Fabric Map: the substrate the planner scores hosts against. Served by `GET /map`.
///
/// Mirrors ce-bench-core's `FabricMap`. The daemon builds it by folding the local node's
/// `/netgraph` + `/atlas` together with the per-node signed `NodeProfile`s ce-bench gossips; the
/// planner reads only this assembled view.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FabricMap {
    /// Every known node, with capacity + optional measured profile.
    #[serde(default)]
    pub nodes: Vec<FabricNode>,
    /// Fused undirected latency edges.
    #[serde(default)]
    pub edges: Vec<MeshEdge>,
    /// The vantage node id this map was assembled from (latency origin for predictions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<NodeId>,
    /// Network-wide scoreboard.
    #[serde(default)]
    pub stats: FabricStats,
    /// The verifiable public-randomness reference (node `/beacon`) captured at assembly time; seeds
    /// the planner's deterministic tie-break so `(map, spec) -> plan` is replayable (§6).
    #[serde(default)]
    pub beacon: BeaconRef,
    /// Unix milliseconds the map was assembled.
    #[serde(default)]
    pub assembled_at_ms: u64,
}

impl FabricMap {
    /// Look up a node's row by id.
    pub fn node(&self, node_id: &str) -> Option<&FabricNode> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobspec_minimal_roundtrips() {
        let json = r#"{"cpuCores":1,"memMb":256}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.cpu_cores, 1);
        assert_eq!(spec.mem_mb, 256);
        assert!(spec.payer.is_none());
        assert!(spec.require_tags.is_empty());
        assert!(!spec.allow_self);
        // Round-trips back to camelCase, omitting None/empty fields.
        let back = serde_json::to_string(&spec).unwrap();
        assert!(back.contains("\"cpuCores\":1"));
        assert!(back.contains("\"memMb\":256"));
        assert!(!back.contains("payer"));
    }

    #[test]
    fn jobspec_full_camelcase_fields() {
        let json = r#"{
            "payer":"me","k":3,"cpuCores":2,"memMb":512,
            "requireTags":["docker","gpu"],"allowSelf":true,
            "objective":"latency","rttSoftCapMs":250,
            "priceCapBaseUnits":"1000000000000000000000",
            "redundancy":"verify","maxShare":0.34,
            "demand":{"vramMb":{"weight":1,"target":8000}},
            "recentPlacements":{"eu-a":8,"us-a":2}
        }"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.payer.as_deref(), Some("me"));
        assert_eq!(spec.k, Some(3));
        assert_eq!(spec.objective, Some(Objective::Latency));
        assert_eq!(spec.redundancy, Some(Redundancy::Policy(RedundancyPolicy::Verify)));
        assert_eq!(spec.price_cap_base_units.as_deref(), Some("1000000000000000000000"));
        let vram = spec.demand.as_ref().unwrap().vram_mb.unwrap();
        assert_eq!(vram.target, 8000.0);
        assert_eq!(spec.recent_placements.as_ref().unwrap().get("eu-a"), Some(&8.0));
    }

    #[test]
    fn redundancy_union_string_or_number() {
        // string arms
        let none: Redundancy = serde_json::from_str("\"none\"").unwrap();
        assert_eq!(none, Redundancy::Policy(RedundancyPolicy::None));
        let verify: Redundancy = serde_json::from_str("\"verify\"").unwrap();
        assert_eq!(verify, Redundancy::Policy(RedundancyPolicy::Verify));
        // number arm => target confidence
        let conf: Redundancy = serde_json::from_str("0.99").unwrap();
        assert_eq!(conf, Redundancy::Confidence(0.99));
        // serialize round-trips
        assert_eq!(serde_json::to_string(&verify).unwrap(), "\"verify\"");
        assert_eq!(serde_json::to_string(&conf).unwrap(), "0.99");
    }

    #[test]
    fn weights_serialize_camelcase() {
        let w = Weights::for_objective(Objective::Balanced);
        let v = serde_json::to_value(w).unwrap();
        // The wire form is camelCase (wL/wB/wT/wP/wD), matching the JS OBJECTIVE_WEIGHTS keys.
        assert_eq!(v["wL"], 0.25);
        assert_eq!(v["wD"], 0.15);
        // The RAW objective table is deliberately NOT convex (balanced sums to 0.85 here); normalization
        // of the convex axes to 1 happens in `scorer::resolve_weights` (see its own test), not the table.
        let raw_sum = w.w_l + w.w_b + w.w_t + w.w_p;
        assert!((raw_sum - 0.85).abs() < 1e-9, "raw balanced convex axes sum to 0.85 pre-normalization, got {raw_sum}");
    }

    #[test]
    fn objective_default_is_balanced() {
        assert_eq!(Objective::default(), Objective::Balanced);
        assert_eq!(Objective::default().as_str(), "balanced");
    }

    #[test]
    fn placement_plan_roundtrips() {
        let plan = PlacementPlan {
            targets: vec![PlanTarget {
                node_id: "us-a".into(),
                score: 0.87,
                rtt_ms: 5.0,
                bench_fit: 0.9,
                trust: 0.4,
                groups: Groups {
                    operator: "us-a".into(),
                    asn: "64500".into(),
                    region: "r0".into(),
                    cluster: "us-a|64500|r0".into(),
                },
                replica: 0,
            }],
            effective_k: 1,
            requested_k: 1,
            beacon: BeaconRef { height: 999, hash: "deadbeef".into() },
            weights: Weights::for_objective(Objective::Latency),
            objective: "latency".into(),
            relaxed: vec![],
            shortfall: 0,
            rejected: vec![RejectedHost { node_id: "eu-a".into(), reason: "low_score".into() }],
            assembled_at_ms: 1,
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"effectiveK\":1"));
        assert!(json.contains("\"benchFit\":0.9"));
        let back: PlacementPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.targets[0].node_id, "us-a");
        assert_eq!(back.rejected[0].reason, "low_score");
    }

    #[test]
    fn fabric_map_mirrors_bench_snake_case() {
        let json = r#"{
            "nodes":[{"node_id":"us-a","capacity":{"node_id":"us-a","cpu_cores":8,"mem_mb":16000,"running_jobs":0,"last_seen_secs":10,"tags":["docker","gpu"]},
                      "profile":{"node_id":"us-a","cpu":{"cores":8,"gflops_fp32":400},"gpus":[{"backend":"Cuda","vram_mb":24000,"fp16_tflops":80}],"llm":{"tokens_per_sec":120},"runtime":{"os":"linux","kind":"Native"}}}],
            "edges":[{"a":"us-a","b":"us-b","rtt_ms":5.3,"samples":12}],
            "stats":{"nodes":1,"cpu_cores":8,"cpu_gflops":400,"gpus":1,"gpu_vram_mb":24000,"gpu_tflops":80,"tokens_per_sec":120,"storage_free_gb":0,"perf_score":1,
                     "mesh":{"median_rtt_ms":5.3,"reachable_frac":1,"regions":1},"by_kind":{"native":1,"container":0,"browser":0},"computed_at":100},
            "assembled_at_ms":123
        }"#;
        let map: FabricMap = serde_json::from_str(json).unwrap();
        assert_eq!(map.nodes.len(), 1);
        let n = map.node("us-a").unwrap();
        assert_eq!(n.capacity.cpu_cores, 8);
        assert_eq!(n.profile.as_ref().unwrap().gpus[0].vram_mb, 24000.0);
        assert_eq!(map.edges[0].rtt_ms, 5.3);
        assert_eq!(map.stats.by_kind.native, 1);
        assert_eq!(map.stats.mesh.regions, 1);
    }

    #[test]
    fn dispatch_result_roundtrips() {
        let dr = DispatchResult {
            dispatched: vec![Dispatched {
                node_id: "us-a".into(),
                replica: 0,
                ok: true,
                job_id: Some("job-1".into()),
                error: None,
            }],
            placed: 1,
            assembled_at_ms: 5,
        };
        let v = serde_json::to_value(&dr).unwrap();
        assert_eq!(v["dispatched"][0]["nodeId"], "us-a");
        assert_eq!(v["dispatched"][0]["jobId"], "job-1");
        assert!(v["dispatched"][0].get("error").is_none());
    }
}
