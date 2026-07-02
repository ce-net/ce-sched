//! ce-sched-daemon — the per-machine ce-sched agent.
//!
//! Mirrors the ce-bench daemon shape (see `../../ce-bench/docs/RUST-DAEMON-ARCH.md`): exactly one
//! instance per node, supervised by ce-appmgr (`daemon = true`), binding a fixed local API port so
//! every SDK on the machine finds it deterministically. It does no placement math itself — that lives
//! in the pure [`ce_sched_core`] crate; the daemon only gathers the [`FabricMap`] and serves it.
//!
//! ## API (binds `127.0.0.1:8856` by default; override with `$CE_SCHED_ADDR`)
//!
//! | Method | Path     | Body              | Returns          |
//! |--------|----------|-------------------|------------------|
//! | GET    | `/health`| —                 | `{ "status": "ok", .. }` |
//! | GET    | `/map`   | —                 | [`FabricMap`]    |
//! | POST   | `/plan`  | [`JobSpec`]       | [`PlacementPlan`]|
//! | POST   | `/place` | [`PlaceRequest`]  | [`DispatchResult`]|
//!
//! ## Where the map comes from
//! The base map is assembled from the local node's read-substrate (`/atlas` capacity + `/netgraph`
//! edges + `/beacon` randomness + per-node `/history` reputation via [`ce_rs::CeClient`]). When the
//! **local ce-bench daemon** (default `http://127.0.0.1:8855`, override with `$CE_BENCH_ADDR`) is
//! reachable, its richer `GET /map` — the folded mesh gossip of signed `NodeProfile`s plus the
//! network scoreboard — is preferred and merged on top (profiles, stats, gossip-only nodes). When
//! ce-bench is not installed / still warming up the thin node map alone is served, so planning
//! degrades gracefully (atlas-fallback scoring in the core).
//!
//! ## Dispatch
//! `/place` turns a [`PlacementPlan`] into action: for each target it calls the node's `/mesh-deploy`
//! (via [`ce_rs::CeClient::mesh_deploy`]) and reports per-host outcomes.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ce_rs::{Amount, BidSpec, CeClient};
use ce_sched_core::api::{
    BeaconRef, ByKind, DispatchResult, Dispatched, FabricMap, FabricNode, FabricStats, JobSpec,
    MeshEdge, MeshSummary, NodeCapacity, NodeHistory, NodeProfile, PlaceRequest, PlacementPlan,
};
use ce_sched_core::placer::{self, PlanError};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Default local bind address for the ce-sched daemon API.
const DEFAULT_ADDR: &str = "127.0.0.1:8856";
/// Default base URL of the local ce-bench daemon (the Fabric Map source).
const DEFAULT_BENCH_ADDR: &str = "http://127.0.0.1:8855";

/// Shared daemon state: the node client (read-substrate + dispatch) and the ce-bench map source.
#[derive(Clone)]
struct AppState {
    /// Client for the local CE node HTTP API (`:8844`) — read-substrate + `/mesh-deploy`.
    ce: CeClient,
    /// Base URL of the local ce-bench daemon serving the assembled `GET /map`.
    bench_base: String,
    /// Shared reqwest client for talking to the ce-bench daemon.
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = std::env::var("CE_SCHED_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let bench_base = std::env::var("CE_BENCH_ADDR")
        .unwrap_or_else(|_| DEFAULT_BENCH_ADDR.to_string())
        .trim_end_matches('/')
        .to_string();

    let state = AppState {
        ce: CeClient::local(),
        bench_base,
        http: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/map", get(get_map))
        .route("/plan", post(post_plan))
        .route("/place", post(post_place))
        .with_state(Arc::new(state));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("ce-sched-daemon listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Handlers
// ----------------------------------------------------------------------------

/// `GET /health` — liveness + the configured upstreams.
async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": "ce-sched-daemon",
        "version": env!("CARGO_PKG_VERSION"),
        "node_api": state.ce.base_url(),
        "bench_api": state.bench_base,
    }))
}

/// `GET /map` — the assembled Fabric Map the planner scores against.
async fn get_map(State(state): State<Arc<AppState>>) -> Result<Json<FabricMap>, AppError> {
    let map = assemble_map(&state).await?;
    Ok(Json(map))
}

/// `POST /plan` — `JobSpec -> PlacementPlan`. Auto-fills the payer (latency origin) from the local
/// node's `/status` when omitted, assembles the map, then runs the pure planner.
async fn post_plan(
    State(state): State<Arc<AppState>>,
    Json(mut spec): Json<JobSpec>,
) -> Result<Json<PlacementPlan>, AppError> {
    if spec.payer.as_deref().unwrap_or("").is_empty() {
        if let Ok(status) = state.ce.status().await {
            spec.payer = Some(status.node_id);
        }
    }
    let map = assemble_map(&state).await?;
    let plan = placer::plan(&spec, &map).map_err(AppError::from_plan)?;
    Ok(Json(plan))
}

/// `POST /place` — dispatch a plan's targets via the node's `/mesh-deploy`. Each target gets the same
/// [`DeploySpec`](ce_sched_core::api::DeploySpec); per-host outcomes are reported (a failed dispatch
/// does not abort the rest).
async fn post_place(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PlaceRequest>,
) -> Result<Json<DispatchResult>, AppError> {
    let PlaceRequest { plan, deploy } = req;

    // Parse the per-host bid (base-unit decimal string) once.
    let bid = deploy
        .bid
        .parse::<i128>()
        .map(Amount::from_base)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("invalid bid base units: {e}")))?;

    let mut dispatched = Vec::with_capacity(plan.targets.len());
    let mut placed = 0u32;
    for t in &plan.targets {
        let spec = BidSpec {
            image: deploy.image.clone(),
            cmd: deploy.cmd.clone(),
            cpu_cores: deploy.cpu_cores,
            mem_mb: deploy.mem_mb,
            duration_secs: deploy.duration_secs,
            bid,
        };
        match state.ce.mesh_deploy(&t.node_id, &spec, deploy.grant.as_deref()).await {
            Ok(job_id) => {
                placed += 1;
                dispatched.push(Dispatched {
                    node_id: t.node_id.clone(),
                    replica: t.replica,
                    ok: true,
                    job_id: Some(job_id),
                    error: None,
                });
            }
            Err(e) => dispatched.push(Dispatched {
                node_id: t.node_id.clone(),
                replica: t.replica,
                ok: false,
                job_id: None,
                error: Some(e.to_string()),
            }),
        }
    }

    Ok(Json(DispatchResult {
        dispatched,
        placed,
        assembled_at_ms: now_ms(),
    }))
}

// ----------------------------------------------------------------------------
// Fabric Map assembly
// ----------------------------------------------------------------------------

/// Assemble the Fabric Map the planner scores against: the node read-substrate (capacity + edges +
/// beacon + histories — the parts only the node has), preferring the local ce-bench daemon's richer
/// folded map (signed `NodeProfile`s + `FabricStats` + gossip-only nodes) merged on top whenever it
/// is reachable. When ce-bench is unavailable the thin node map alone is returned, so planning
/// degrades gracefully (the core scores profile-less hosts via the atlas fallback).
async fn assemble_map(state: &AppState) -> Result<FabricMap, AppError> {
    let mut map = map_from_node(state).await?;
    match fetch_bench_map(state).await {
        Some(bench) => merge_bench_map(&mut map, bench),
        None => tracing::debug!("ce-bench /map unavailable at {}; serving the thin node map", state.bench_base),
    }
    bridge_unmeasured_nodes(&mut map);
    Ok(map)
}

/// Bridge the substrate's identity split: `/netgraph` keys its measured edges by libp2p PeerId while
/// `/atlas` (and profile gossip) key nodes by hex node id, so no measured edge ever lands on a
/// planning candidate and the engine's reachability gate (§1.5) would exclude every one of them.
/// A fresh atlas/gossip row IS proof of mesh reachability (the node just heard from that peer), so
/// give each candidate the origin lacks a measured edge to a synthetic origin-anchored edge at a
/// conservative estimate: the median of the measured netgraph RTTs (else 50ms), `samples: 1` so any
/// real measurement out-weighs it in the graph's sample-weighted fusion. This is a map-assembly
/// heuristic, not engine semantics; it disappears once the substrate reports node ids on `/netgraph`
/// or ce-bench echo probes populate per-node-id RTTs.
fn bridge_unmeasured_nodes(map: &mut FabricMap) {
    let Some(origin) = map.origin.clone() else { return };
    let mut measured: Vec<f64> = map.edges.iter().map(|e| e.rtt_ms).filter(|r| r.is_finite() && *r > 0.0).collect();
    measured.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let estimate = if measured.is_empty() { 50.0 } else { measured[measured.len() / 2] };
    let now = now_ms() / 1000;
    let new_edges: Vec<MeshEdge> = map
        .nodes
        .iter()
        .filter(|n| n.node_id != origin)
        .filter(|n| !map.edges.iter().any(|e| e.a == n.node_id || e.b == n.node_id))
        .map(|n| MeshEdge {
            a: origin.clone(),
            b: n.node_id.clone(),
            rtt_ms: estimate,
            samples: 1,
            last_seen_secs: now,
        })
        .collect();
    map.edges.extend(new_edges);
}

// ----- the ce-bench `GET /map` wire (ce-bench-core `FabricMap`; only what ce-sched consumes) -----

/// One folded node entry in the ce-bench map: the signed profile is what ce-sched wants, plus the
/// reliability sighting times (a gossip-only node's liveness is when its gossip was last HEARD, not
/// when it last MEASURED — ce-bench re-measures every 300s, which would flip-flop the planner's
/// 180s default staleness gate if `measured_at` were the liveness signal).
#[derive(Debug, Deserialize)]
struct BenchNodeEntry {
    #[serde(default)]
    node_id: String,
    /// ce-bench's `NodeProfile` is a superset of ours (schema/beacon/samples extras); serde ignores
    /// the unknown fields and the shared axes (cpu/gpus/memory/storage/llm/runtime/sig) map 1:1.
    #[serde(default)]
    profile: Option<NodeProfile>,
    #[serde(default)]
    reliability: Option<BenchReliability>,
}

/// The slice of ce-bench's reliability accounting ce-sched reads (`last_seen` = fold-time sighting).
#[derive(Debug, Default, Deserialize)]
struct BenchReliability {
    #[serde(default)]
    last_seen: u64,
}

/// Mesh-health summary (ce-bench `MeshHealth`).
#[derive(Debug, Default, Deserialize)]
struct BenchMesh {
    #[serde(default)]
    median_rtt_ms: f64,
    #[serde(default)]
    reachable_frac: f64,
    #[serde(default)]
    regions: u64,
}

/// Node-kind census (ce-bench `ByKind`).
#[derive(Debug, Default, Deserialize)]
struct BenchByKind {
    #[serde(default)]
    native: u64,
    #[serde(default)]
    container: u64,
    #[serde(default)]
    browser: u64,
}

/// The assembled ce-bench map (ce-bench-core `FabricMap`, snake_case wire). Aggregates live at the
/// top level there; ce-sched folds them into its `FabricStats`.
#[derive(Debug, Default, Deserialize)]
struct BenchMap {
    #[serde(default)]
    nodes: Vec<BenchNodeEntry>,
    #[serde(default)]
    node_count: u64,
    #[serde(default)]
    cpu_cores: u64,
    #[serde(default)]
    cpu_gflops: f64,
    #[serde(default)]
    gpus: u64,
    #[serde(default)]
    gpu_vram_mb: f64,
    #[serde(default)]
    gpu_tflops: f64,
    #[serde(default)]
    tokens_per_sec: f64,
    #[serde(default)]
    storage_free_gb: f64,
    #[serde(default)]
    perf_score: f64,
    #[serde(default)]
    mesh: BenchMesh,
    #[serde(default)]
    by_kind: BenchByKind,
    #[serde(default)]
    computed_at: u64,
}

/// Try the local ce-bench daemon's `GET /map`. Returns `None` on any error so the caller serves the
/// thin node map instead.
async fn fetch_bench_map(state: &AppState) -> Option<BenchMap> {
    let url = format!("{}/map", state.bench_base);
    let resp = state.http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<BenchMap>().await.ok()
}

/// Fold the ce-bench map into the node-substrate map: attach each gossiped signed profile to its
/// node row (appending rows for gossip-only nodes the atlas has not seen, with capacity derived from
/// the profile so feasibility still has something to read), and adopt the network scoreboard.
fn merge_bench_map(map: &mut FabricMap, bench: BenchMap) {
    for entry in bench.nodes {
        if entry.node_id.is_empty() {
            continue;
        }
        match map.nodes.iter_mut().find(|n| n.node_id == entry.node_id) {
            Some(node) => {
                if entry.profile.is_some() {
                    node.profile = entry.profile;
                }
            }
            None => {
                let mut capacity = entry
                    .profile
                    .as_ref()
                    .map(|p| capacity_from_profile(&entry.node_id, p))
                    .unwrap_or_else(|| NodeCapacity { node_id: entry.node_id.clone(), ..Default::default() });
                // Liveness = last gossip sighting when fresher than the profile's measurement time.
                if let Some(r) = &entry.reliability {
                    capacity.last_seen_secs = capacity.last_seen_secs.max(r.last_seen);
                }
                map.nodes.push(FabricNode {
                    node_id: entry.node_id,
                    capacity,
                    profile: entry.profile,
                    history: None,
                });
            }
        }
    }
    map.stats = FabricStats {
        nodes: bench.node_count,
        cpu_cores: bench.cpu_cores,
        cpu_gflops: bench.cpu_gflops,
        gpus: bench.gpus,
        gpu_vram_mb: bench.gpu_vram_mb,
        gpu_tflops: bench.gpu_tflops,
        tokens_per_sec: bench.tokens_per_sec,
        storage_free_gb: bench.storage_free_gb,
        perf_score: bench.perf_score,
        mesh: MeshSummary {
            median_rtt_ms: bench.mesh.median_rtt_ms,
            reachable_frac: bench.mesh.reachable_frac,
            regions: bench.mesh.regions,
        },
        by_kind: ByKind { native: bench.by_kind.native, container: bench.by_kind.container, browser: bench.by_kind.browser },
        computed_at: bench.computed_at,
    };
}

/// Coarse capacity for a gossip-only node (no atlas row): cores/mem from the measured profile,
/// capability tags from the runtime flags, liveness from the profile's measurement time.
fn capacity_from_profile(node_id: &str, p: &NodeProfile) -> NodeCapacity {
    let mut tags = Vec::new();
    for t in [p.runtime.os.as_str(), p.runtime.arch.as_str()] {
        if !t.is_empty() {
            tags.push(t.to_string());
        }
    }
    if p.runtime.docker {
        tags.push("docker".into());
    }
    if p.runtime.wasm {
        tags.push("wasm".into());
    }
    if !p.gpus.is_empty() {
        tags.push("gpu".into());
    }
    NodeCapacity {
        node_id: node_id.to_string(),
        cpu_cores: p.cpu.cores,
        mem_mb: p.memory.total_mb.max(0.0) as u64,
        running_jobs: 0,
        last_seen_secs: p.measured_at,
        tags,
        ask_base_units: None,
    }
}

// ----- the node read-substrate (thin map) -----

/// Thin Fabric Map from the node's `/atlas` (capacity) + `/netgraph` (edges) + `/status` (origin) +
/// `/beacon` (selection randomness) + per-node `/history` (the trust substrate). Measured profiles
/// are ce-bench's contribution, merged on top by [`assemble_map`].
async fn map_from_node(state: &AppState) -> Result<FabricMap, AppError> {
    let atlas = state
        .ce
        .atlas()
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, format!("node /atlas unavailable: {e}")))?;
    let netgraph = state.ce.netgraph().await.unwrap_or_default();
    let origin = state.ce.status().await.ok().map(|s| s.node_id);
    let beacon = state
        .ce
        .beacon()
        .await
        .map(|b| BeaconRef { height: b.height, hash: b.hash })
        .unwrap_or_default();

    let mut nodes = atlas
        .into_iter()
        .map(|e| FabricNode {
            node_id: e.node_id.clone(),
            capacity: NodeCapacity {
                node_id: e.node_id,
                cpu_cores: e.cpu_cores,
                mem_mb: e.mem_mb as u64,
                running_jobs: e.running_jobs,
                last_seen_secs: e.last_seen_secs,
                tags: e.tags,
                ask_base_units: None,
            },
            profile: None,
            history: None,
        })
        .collect::<Vec<_>>();

    attach_histories(state, &mut nodes).await;

    // /netgraph edges are this node's measured RTT to each peer (keyed by libp2p PeerId). Anchor them
    // at the origin so the planner's latency-from-payer queries have a measured sample.
    let edges = match &origin {
        Some(o) => netgraph
            .into_iter()
            .map(|edge| MeshEdge {
                a: o.clone(),
                b: edge.peer,
                rtt_ms: edge.rtt_ms,
                samples: edge.samples,
                last_seen_secs: edge.last_seen_secs,
            })
            .collect(),
        None => Vec::new(),
    };

    Ok(FabricMap {
        nodes,
        edges,
        origin,
        stats: Default::default(),
        beacon,
        assembled_at_ms: now_ms(),
    })
}

/// Attach each node's on-chain `/history` facts (concurrent, fault-tolerant: a failed lookup leaves
/// `history: None` and the trust score treats the host as a stranger). Fetched raw so the fields the
/// scorer reads that `ce_rs::NodeHistory` does not carry (`recent_earned`) survive the trip.
async fn attach_histories(state: &AppState, nodes: &mut [FabricNode]) {
    let mut set = tokio::task::JoinSet::new();
    for (i, node) in nodes.iter().enumerate() {
        let url = format!("{}/history/{}", state.ce.base_url(), node.node_id);
        let http = state.http.clone();
        set.spawn(async move {
            let hist = async {
                let resp = http.get(&url).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                resp.json::<NodeHistory>().await.ok()
            }
            .await;
            (i, hist)
        });
    }
    while let Some(res) = set.join_next().await {
        if let Ok((i, hist)) = res {
            nodes[i].history = hist;
        }
    }
}

// ----------------------------------------------------------------------------
// Error + helpers
// ----------------------------------------------------------------------------

/// A handler error rendered as `(status, { "error": msg })`.
struct AppError(StatusCode, String);

impl AppError {
    /// Map a pure-planner [`PlanError`] to an HTTP status. (An over-constrained request is not an
    /// error — the planner returns a short/empty plan with a `shortfall` instead.)
    fn from_plan(e: PlanError) -> Self {
        match e {
            PlanError::MissingPayer => AppError(StatusCode::BAD_REQUEST, e.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Unix milliseconds now.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact ce-bench daemon `GET /map` wire (as served by the stub at :8855) must parse.
    #[test]
    fn bench_map_wire_parses() {
        let empty = r#"{"nodes":[],"node_count":0,"cpu_cores":0,"cpu_gflops":0.0,"gpus":0,"gpu_vram_mb":0,"gpu_tflops":0.0,"tokens_per_sec":0.0,"storage_free_gb":0,"perf_score":0.0,"mesh":{"median_rtt_ms":0.0,"reachable_frac":0.0,"regions":0},"by_kind":{"native":0,"container":0,"browser":0},"computed_at":0}"#;
        let m: BenchMap = serde_json::from_str(empty).unwrap();
        assert!(m.nodes.is_empty());

        // A populated entry: ce-bench's NodeProfile is a superset (schema/beacon/samples extras are
        // ignored); the shared axes land in our NodeProfile.
        let rich = r#"{"nodes":[{"node_id":"n1","profile":{"node_id":"n1","schema":1,"measured_at":9,"beacon_height":5,"beacon_hash":"ab","bench_app":"ce-bench-rs@0.0.1","cpu":{"cores":8,"threads":16,"gflops_fp32":400.0,"mem_bw_gbps":50.0},"gpus":[{"model":"X","backend":"Cuda","vram_mb":24000.0,"fp16_tflops":80.0}],"memory":{"total_mb":16000.0,"available_mb":12000.0},"storage":{"total_gb":1.0,"free_gb":1.0,"read_mbps":1.0,"write_mbps":1.0},"llm":{"ref_model":"m","tokens_per_sec":120.0,"ctx_tokens":4096},"runtime":{"os":"linux","arch":"x86_64","docker":true,"gvisor":false,"wasm":true,"webgpu":false,"kind":"Native"},"samples":[]},"reliability":{"first_seen":1,"last_seen":2,"samples":3,"uptime_frac":0.9,"score":0.8},"region":0}],"node_count":1,"mesh":{"median_rtt_ms":5.0,"reachable_frac":1.0,"regions":1},"by_kind":{"native":1,"container":0,"browser":0},"computed_at":7}"#;
        let m: BenchMap = serde_json::from_str(rich).unwrap();
        let p = m.nodes[0].profile.as_ref().unwrap();
        assert_eq!(p.cpu.cores, 8);
        assert_eq!(p.gpus[0].vram_mb, 24000.0);
        assert_eq!(p.runtime.kind.as_deref(), Some("Native"));
    }

    /// Merging attaches profiles to atlas rows, appends gossip-only nodes with derived capacity,
    /// and adopts the scoreboard — while the node-substrate parts (capacity/edges/beacon) survive.
    #[test]
    fn merge_bench_map_enriches_the_thin_map() {
        let mut map = FabricMap {
            nodes: vec![FabricNode {
                node_id: "a".into(),
                capacity: NodeCapacity { node_id: "a".into(), cpu_cores: 4, ..Default::default() },
                profile: None,
                history: None,
            }],
            beacon: BeaconRef { height: 3, hash: "cc".into() },
            ..Default::default()
        };
        let bench = BenchMap {
            nodes: vec![
                BenchNodeEntry {
                    node_id: "a".into(),
                    profile: Some(NodeProfile { node_id: "a".into(), ..Default::default() }),
                    reliability: None,
                },
                BenchNodeEntry {
                    node_id: "gossip-only".into(),
                    profile: Some(NodeProfile {
                        node_id: "gossip-only".into(),
                        measured_at: 42,
                        cpu: ce_sched_core::api::CpuProfile { cores: 2, ..Default::default() },
                        runtime: ce_sched_core::api::RuntimeInfo { os: "linux".into(), wasm: true, ..Default::default() },
                        ..Default::default()
                    }),
                    reliability: Some(BenchReliability { last_seen: 90 }),
                },
            ],
            node_count: 2,
            mesh: BenchMesh { median_rtt_ms: 5.0, reachable_frac: 1.0, regions: 1 },
            ..Default::default()
        };
        merge_bench_map(&mut map, bench);
        assert_eq!(map.nodes.len(), 2);
        assert!(map.nodes[0].profile.is_some(), "atlas row gains the gossiped profile");
        assert_eq!(map.nodes[0].capacity.cpu_cores, 4, "atlas capacity is authoritative, not overwritten");
        let g = &map.nodes[1];
        assert_eq!(g.capacity.cpu_cores, 2, "gossip-only node derives capacity from its profile");
        assert_eq!(g.capacity.last_seen_secs, 90, "liveness = gossip sighting, fresher than measured_at");
        assert!(g.capacity.tags.iter().any(|t| t == "wasm"));
        assert_eq!(map.stats.nodes, 2, "scoreboard adopted from ce-bench");
        assert_eq!(map.stats.mesh.regions, 1);
        assert_eq!(map.beacon.height, 3, "node-substrate beacon survives the merge");
    }
}
