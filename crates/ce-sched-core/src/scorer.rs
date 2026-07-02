//! Per-candidate scalar scoring — pure functions, no I/O.
//!
//! Rust port of the JS `src/scorer.js`. Turns a feasibility-filtered [`Candidate`] + the
//! [`JobSpec`](crate::api::JobSpec) into a `[0,1]` static score from four convex-blended axes
//! (latency, benchFit, trust, price). The contextual diversity penalty is NOT here — it depends on
//! what is already chosen and lives in [`crate::vendor`] / [`crate::placer`].
//!
//! CONTRACT (see `docs/placement-design.md` §2 and the JS module):
//!
//! - [`resolve_weights`] — objective defaults + spec override, convex axes normalized to 1, `wD`
//!   carried verbatim.
//! - [`clamp01`] — clamp a score to `[0,1]`.
//! - [`latency_score`] — §2.1 `1 - rtt/rttSoftCapMs`, prefer measured (candidate-carried first).
//! - [`bench_fit_score`] — §2.2 demand-weighted saturating match ([`BenchFit`] with provenance).
//! - [`trust_score`] — §2.3 log-saturating delivered-work + §9 benchmark cross-check ([`Trust`]).
//! - [`price_score`] — §2.4 128-bit money math, ratio-only float.
//! - [`static_score`] — the convex blend + the runtime-kind factor ([`StaticScore`]).
//!
//! Money rule: `price_score` (and the trust recency check) parse `ask_base_units` /
//! `price_cap_base_units` / `earned` as `i128` base units; the ONLY float produced is the `[0,1]`
//! ranking ratio. A base-unit string is never coerced straight to `f64` for money math. (Amounts
//! beyond `i128` — ~1.7e20 credits — fail the parse and degrade to the neutral score, where JS
//! BigInt is unbounded; noted deviation.)

use crate::api::{Demand, DemandAxis, JobSpec, NodeProfile, Objective, Weights};
use crate::graph::LatencyView;
use crate::placer::Candidate;

/// Clamp a number to `[0,1]`. NaN maps to 0. Shared by the scorer/vendor math.
pub fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else if x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

/// Finite non-negative `x`, else the fallback (the JS `numOr`).
pub(crate) fn num_or(x: Option<f64>, fallback: f64) -> f64 {
    match x {
        Some(v) if v.is_finite() && v >= 0.0 => v,
        _ => fallback,
    }
}

/// Parse a base-unit money STRING to `i128` without ever going through a float. Tolerant of empty
/// and malformed input (→ 0) so trust scoring degrades gracefully (the JS `toBig`).
pub(crate) fn to_big(x: &str) -> i128 {
    x.trim().parse::<i128>().unwrap_or(0)
}

// ----------------------------------------------------------------------------
// Weights (§2.5)
// ----------------------------------------------------------------------------

/// Resolve the blend [`Weights`] for a request (§2.5). Starts from the objective's defaults, applies
/// any caller `spec.weights` override (per-axis, ignoring non-finite/negative), then normalizes the
/// four convex axes (`wL+wB+wT+wP`) to sum to 1. `wD` is the diversity-penalty coefficient and is
/// carried through verbatim, never folded into the normalization. A degenerate all-zero convex blend
/// falls back to an equal split so scoring still discriminates.
pub fn resolve_weights(spec: &JobSpec) -> Weights {
    let objective = spec.objective.unwrap_or(Objective::Balanced);
    let base = Weights::for_objective(objective);
    let ov = spec.weights;

    let pick = |over: f64, default: f64| -> f64 {
        if over.is_finite() && over >= 0.0 { over } else { default }
    };

    let (mut w_l, mut w_b, mut w_t, mut w_p, w_d) = match ov {
        Some(o) => (
            pick(o.w_l, base.w_l),
            pick(o.w_b, base.w_b),
            pick(o.w_t, base.w_t),
            pick(o.w_p, base.w_p),
            pick(o.w_d, base.w_d),
        ),
        None => (base.w_l, base.w_b, base.w_t, base.w_p, base.w_d),
    };

    let sum = w_l + w_b + w_t + w_p;
    if sum > 0.0 {
        w_l /= sum;
        w_b /= sum;
        w_t /= sum;
        w_p /= sum;
    } else {
        w_l = 0.25;
        w_b = 0.25;
        w_t = 0.25;
        w_p = 0.25;
    }

    Weights { w_l, w_b, w_t, w_p, w_d }
}

// ----------------------------------------------------------------------------
// latency (§2.1)
// ----------------------------------------------------------------------------

/// Latency sub-score in `[0,1]`: closer to the payer is better. Ground-truth measured RTT is
/// preferred over the embedding prediction; beyond `spec.rtt_soft_cap_ms` the score floors at 0 (the
/// host is still feasible, it just wins only on other axes). An unreachable host (infinite RTT)
/// scores 0.
///
/// The candidate carries a pre-resolved `rtt_ms` (set by the placer's feasibility pass); when finite
/// it is used verbatim, otherwise the graph is queried (measured first, then predicted).
pub fn latency_score(candidate: &Candidate, spec: &JobSpec, graph: Option<&dyn LatencyView>) -> f64 {
    let cap = num_or(spec.rtt_soft_cap_ms, 250.0);
    if !(cap > 0.0) {
        return 0.0;
    }
    let rtt = resolve_rtt(candidate, spec, graph);
    if !rtt.is_finite() {
        return 0.0;
    }
    clamp01(1.0 - rtt / cap)
}

/// Resolve the RTT (ms) from the payer to a candidate: a candidate-carried value first (the placer
/// sets it), then a direct measured graph sample, then the embedding prediction. Infinity if nothing
/// relates them.
fn resolve_rtt(candidate: &Candidate, spec: &JobSpec, graph: Option<&dyn LatencyView>) -> f64 {
    if candidate.rtt_ms.is_finite() {
        return candidate.rtt_ms;
    }
    let (Some(graph), Some(payer)) = (graph, spec.payer.as_deref().filter(|p| !p.is_empty())) else {
        return f64::INFINITY;
    };
    match graph.measured_rtt(payer, &candidate.node_id) {
        Some(m) if m.is_finite() => m,
        _ => graph.predicted_rtt(payer, &candidate.node_id),
    }
}

// ----------------------------------------------------------------------------
// benchFit (§2.2)
// ----------------------------------------------------------------------------

/// The seven benchmark axes of the demand vector (the JS `PROFILE_AXIS` keys, in `Demand` field order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileAxis {
    Gflops,
    MemBwGbps,
    VramMb,
    Fp16Tflops,
    TokensPerSec,
    DiskReadMbps,
    DiskWriteMbps,
}

/// Read one axis from a measured [`NodeProfile`] (the JS `PROFILE_AXIS` readers).
fn profile_axis(axis: ProfileAxis, p: &NodeProfile) -> f64 {
    let num = |x: f64| if x.is_finite() { x } else { 0.0 };
    match axis {
        ProfileAxis::Gflops => num(p.cpu.gflops_fp32),
        ProfileAxis::MemBwGbps => num(p.cpu.mem_bw_gbps),
        ProfileAxis::VramMb => p.gpus.iter().map(|g| num(g.vram_mb)).fold(0.0, f64::max),
        ProfileAxis::Fp16Tflops => p.gpus.iter().map(|g| num(g.fp16_tflops)).sum(),
        ProfileAxis::TokensPerSec => num(p.llm.tokens_per_sec),
        ProfileAxis::DiskReadMbps => num(p.storage.read_mbps),
        ProfileAxis::DiskWriteMbps => num(p.storage.write_mbps),
    }
}

/// The declared demand axes, in stable `Demand` field order (JS iterated `Object.keys(demand)`).
fn demand_axes(demand: &Demand) -> Vec<(ProfileAxis, DemandAxis)> {
    let pairs = [
        (ProfileAxis::Gflops, demand.gflops),
        (ProfileAxis::MemBwGbps, demand.mem_bw_gbps),
        (ProfileAxis::VramMb, demand.vram_mb),
        (ProfileAxis::Fp16Tflops, demand.fp16_tflops),
        (ProfileAxis::TokensPerSec, demand.tokens_per_sec),
        (ProfileAxis::DiskReadMbps, demand.disk_read_mbps),
        (ProfileAxis::DiskWriteMbps, demand.disk_write_mbps),
    ];
    pairs.into_iter().filter_map(|(a, d)| d.filter(|d| d.weight > 0.0).map(|d| (a, d))).collect()
}

/// Where a benchFit value came from (JS `source: "profile" | "atlas" | "none"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitSource {
    /// Read from the signed measured profile.
    Profile,
    /// Estimated from the coarse atlas signal (cores/mem/tags); confidence < 1.
    Atlas,
    /// No demand was declared (nothing was asked).
    None,
}

/// The benchFit sub-score with its provenance (the trust term consumes `confidence`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BenchFit {
    pub score: f64,
    pub source: FitSource,
    pub confidence: f64,
}

/// benchFit: the demand-weighted, saturating match of the candidate's measured profile to the job's
/// demand vector. Each per-axis fit saturates at 1.0 once the host clears the job's `target`
/// ("enough") bar — surplus hardware earns no extra fit credit.
///
/// ```text
/// fit_a    = clamp01( profile[a] / target_a )
/// benchFit = Σ weight_a · fit_a / Σ weight_a
/// ```
///
/// With no demand declared, benchFit is neutral (0.5) — the axis simply does not discriminate. When
/// the candidate has no signed profile, the coarse atlas signal is mapped to estimated axes and
/// `source = Atlas` with `confidence < 1`; the trust term consumes that confidence so unverified
/// hardware is discounted there, not double-counted here.
pub fn bench_fit_score(candidate: &Candidate, spec: &JobSpec) -> BenchFit {
    let axes = spec.demand.as_ref().map(demand_axes).unwrap_or_default();

    // No demand declared: nothing to discriminate on. Neutral fit, full confidence (we asked nothing).
    if axes.is_empty() {
        let source = if candidate.profile.is_some() { FitSource::Profile } else { FitSource::None };
        return BenchFit { score: 0.5, source, confidence: 1.0 };
    }

    let (source, confidence) = match candidate.profile {
        // Measured hardware is trusted; atlas estimates are coarse → discounted confidence.
        Some(_) => (FitSource::Profile, 1.0),
        None => (FitSource::Atlas, 0.6),
    };

    let mut num = 0.0;
    let mut den = 0.0;
    for (axis, d) in axes {
        if !(d.weight > 0.0) || !(d.target > 0.0) {
            continue;
        }
        let value = match &candidate.profile {
            Some(p) => profile_axis(axis, p),
            None => atlas_axis_estimate(axis, candidate),
        };
        let fit = clamp01(value / d.target);
        num += d.weight * fit;
        den += d.weight;
    }
    let score = if den > 0.0 { num / den } else { 0.5 };
    BenchFit { score: clamp01(score), source, confidence }
}

/// Coarse atlas fallback for a benchmark axis when no signed profile exists. Derives a rough
/// estimate from `cpu_cores`, `mem_mb`, and capability tags. Deliberately conservative — the
/// discounted confidence (0.6) returned alongside is what flags this as unverified.
fn atlas_axis_estimate(axis: ProfileAxis, candidate: &Candidate) -> f64 {
    let cap = &candidate.capacity;
    let cores = cap.cpu_cores as f64;
    let mem_mb = cap.mem_mb as f64;
    let has_gpu = cap.tags.iter().any(|t| t.to_lowercase() == "gpu");

    match axis {
        // ~8 GFLOPS/core fp32 is a conservative modern-core estimate.
        ProfileAxis::Gflops => cores * 8.0,
        // ~6 GB/s per core as a rough shared-bus estimate.
        ProfileAxis::MemBwGbps => cores * 6.0,
        // Without a profile we cannot know VRAM; assume a modest card iff tagged gpu, else 0.
        ProfileAxis::VramMb => if has_gpu { 8000.0 } else { 0.0 },
        ProfileAxis::Fp16Tflops => if has_gpu { 10.0 } else { 0.0 },
        // LLM throughput is gpu-gated; rough token/s guess only when tagged.
        ProfileAxis::TokensPerSec => if has_gpu { 20.0 } else { 0.0 },
        // Disk throughput unknown from atlas; assume commodity SSD if the node hosts at all.
        ProfileAxis::DiskReadMbps => if mem_mb > 0.0 { 500.0 } else { 0.0 },
        ProfileAxis::DiskWriteMbps => if mem_mb > 0.0 { 400.0 } else { 0.0 },
    }
}

// ----------------------------------------------------------------------------
// trust (§2.3 + §9 cross-check)
// ----------------------------------------------------------------------------

/// The trust sub-score plus the §9 benchmark-suspect flag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trust {
    pub score: f64,
    pub benchmark_suspect: bool,
}

/// trust: a reputation weight in `[0,1]` derived from the candidate's on-chain `/history` facts.
///
/// ```text
/// delivered    = jobs_hosted + heartbeats_hosted            (proven hosting work)
/// base         = log1p(delivered) / log1p(trustSaturation)  (log-saturating; default sat 50)
/// recencyBoost = recently-active hosts weighted up to ~1.0, dormant down toward ~0.75
/// trust        = clamp01( base · recencyBoost · profileConfidence )
/// ```
///
/// §9 benchmark cross-check: a host that *claims* a large profile (high GFLOPS / VRAM / tokens/s)
/// but has implausibly little delivered work for that claim is flagged `benchmark_suspect` and its
/// trust is floored toward 0. A host with no profile and no history is simply new (low trust, not
/// suspect).
pub fn trust_score(candidate: &Candidate, spec: &JobSpec) -> Trust {
    let sat = num_or(spec.trust_saturation, 50.0);
    let h = candidate.history.as_ref();

    let delivered = h.map(|h| (h.jobs_hosted + h.heartbeats_hosted) as f64).unwrap_or(0.0);
    let denom = (if sat > 0.0 { sat } else { 50.0 }).ln_1p();
    let base = if denom > 0.0 { delivered.ln_1p() / denom } else { 0.0 };

    // Recency: a node still earning recently is more trustworthy than a long-dormant one. earned is
    // a base-unit STRING — compare as i128, never as a float. recent>0 ⇒ full boost; some all-time
    // earnings but nothing recent ⇒ mild discount; nothing ever ⇒ neutral (base already reflects it).
    let mut recency_boost = 1.0;
    if let Some(h) = h {
        let earned = to_big(&h.earned);
        let recent = to_big(&h.recent_earned);
        if earned > 0 {
            recency_boost = if recent > 0 { 1.0 } else { 0.75 };
        }
    }

    // Profile confidence: unverified (atlas-only) hardware is discounted here so it is counted once.
    let confidence = if candidate.profile.is_some() { 1.0 } else { 0.85 };

    let mut score = clamp01(base * recency_boost * confidence);

    // §9 cross-check: does the *claimed* profile imply far more capability than delivered work supports?
    let benchmark_suspect = is_benchmark_suspect(candidate, delivered);
    if benchmark_suspect {
        // Floor trust hard — a suspect claim should not win on reputation. Keep a sliver only if
        // there is genuine delivered work (the claim is suspect, not the node necessarily fraudulent).
        score = score.min(if delivered > 0.0 { 0.1 } else { 0.0 });
    }

    Trust { score: clamp01(score), benchmark_suspect }
}

/// §9 heuristic: a high-end *claimed* profile with implausibly little delivered work is suspect. We
/// compute a coarse "claim tier" from the signed profile and require a minimum amount of delivered
/// work to corroborate it. No profile ⇒ nothing to over-claim ⇒ not suspect (just unverified, handled
/// by confidence). A node that has actually done the work is never suspect.
fn is_benchmark_suspect(candidate: &Candidate, delivered: f64) -> bool {
    let Some(p) = &candidate.profile else { return false };

    let gflops = profile_axis(ProfileAxis::Gflops, p);
    let vram = profile_axis(ProfileAxis::VramMb, p);
    let tps = profile_axis(ProfileAxis::TokensPerSec, p);
    let tflops = profile_axis(ProfileAxis::Fp16Tflops, p);

    // A "big claim" = a high-end GPU/LLM tier (lots of VRAM, fp16 TFLOPS, or token throughput) or an
    // unusually large CPU GFLOPS figure. Tiers chosen to flag datacenter-grade claims, not laptops.
    let big_claim = vram >= 24000.0 || tflops >= 50.0 || tps >= 100.0 || gflops >= 2000.0;
    if !big_claim {
        return false;
    }

    // To corroborate a big claim we want a floor of delivered hosting work. Scale the floor with the
    // size of the claim so the very largest claims need the most proof.
    let required_delivered = if vram >= 48000.0 || tflops >= 150.0 { 20.0 } else { 5.0 };
    delivered < required_delivered
}

// ----------------------------------------------------------------------------
// price (§2.4) — 128-bit money, ratio-only float
// ----------------------------------------------------------------------------

/// price: cheaper is better, normalized against `spec.price_cap_base_units`. All money math is
/// `i128` over base-unit strings; the ONLY float produced is the final `[0,1]` ranking ratio (never
/// an amount, never written back as money). A host that advertises no ask gets
/// `spec.default_price_score` (neutral 0.5).
///
/// ```text
/// price = clamp01( 1 - ask / priceCap )     // ask, priceCap as i128 base units
/// ```
pub fn price_score(candidate: &Candidate, spec: &JobSpec) -> f64 {
    let default_score = num_or(spec.default_price_score, 0.5);

    // No advertised price → neutral.
    let Some(ask) = candidate.ask_base_units.as_deref().filter(|s| !s.is_empty()) else {
        return clamp01(default_score);
    };
    // No cap to normalize against → cannot rank on price; neutral.
    let Some(cap_str) = spec.price_cap_base_units.as_deref().filter(|s| !s.is_empty()) else {
        return clamp01(default_score);
    };

    // Malformed money strings → do not guess; neutral.
    let (Ok(ask_big), Ok(cap_big)) = (ask.trim().parse::<i128>(), cap_str.trim().parse::<i128>()) else {
        return clamp01(default_score);
    };
    if cap_big <= 0 {
        return clamp01(default_score);
    }
    if ask_big <= 0 {
        return 1.0; // free is the best possible price.
    }
    if ask_big >= cap_big {
        return 0.0; // at/over the cap contributes nothing.
    }

    // ratio = ask / cap in [0,1). Compute with extra integer precision then convert ONCE to a float
    // ratio (this float is a ranking score, not money). Near the i128 ceiling the scaled multiply
    // could overflow, so degrade to a coarser (still integer) division.
    const SCALE: i128 = 1_000_000;
    let scaled = match ask_big.checked_mul(SCALE) {
        Some(x) => x / cap_big,
        None => ask_big / (cap_big / SCALE).max(1),
    };
    let ratio = scaled as f64 / SCALE as f64;
    clamp01(1.0 - ratio)
}

// ----------------------------------------------------------------------------
// static blend (§2)
// ----------------------------------------------------------------------------

/// Per-axis score parts, surfaced for auditability (the explorer's breakdown).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreParts {
    pub latency: f64,
    pub bench_fit: f64,
    pub trust: f64,
    pub price: f64,
    /// Runtime-kind + locality multiplier applied after the convex blend.
    pub runtime_factor: f64,
}

/// The full static score breakdown for one candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticScore {
    pub score: f64,
    pub parts: ScoreParts,
    /// benchFit provenance (source + confidence).
    pub bench_fit: BenchFit,
    pub benchmark_suspect: bool,
}

/// The convex blend of the four static axes (latency, benchFit, trust, price). This is the single
/// entry the placer calls per candidate. The contextual diversity penalty is intentionally NOT
/// here — it depends on what is already chosen and is applied by the placer using vendor.rs.
///
/// ```text
/// score = (wL·latency + wB·benchFit + wT·trust + wP·price) · runtimeFactor
/// ```
///
/// The runtime-kind factor downranks browser-WASM hosts (~5-10x slower execution, harder when the
/// job is throughput-bound) and boosts the payer's own node when it is itself a candidate (running
/// locally avoids the WAN hop; only reachable with `allow_self`). Applied AFTER the convex blend so
/// the axis parts stay interpretable; surfaced in `parts.runtime_factor` for auditability.
pub fn static_score(candidate: &Candidate, spec: &JobSpec, graph: Option<&dyn LatencyView>) -> StaticScore {
    let w = resolve_weights(spec);

    let latency = latency_score(candidate, spec, graph);
    let bench = bench_fit_score(candidate, spec);
    let trust = trust_score(candidate, spec);
    let price = price_score(candidate, spec);

    let blended = clamp01(w.w_l * latency + w.w_b * bench.score + w.w_t * trust.score + w.w_p * price);
    let runtime_factor = runtime_kind_factor(candidate, spec.payer.as_deref(), &w);
    let score = clamp01(blended * runtime_factor);

    StaticScore {
        score,
        parts: ScoreParts { latency, bench_fit: bench.score, trust: trust.score, price, runtime_factor },
        bench_fit: bench,
        benchmark_suspect: trust.benchmark_suspect,
    }
}

/// Browser-WASM throughput is ~5-10x native; base multiplier applied to a browser candidate's score.
pub const BROWSER_RUNTIME_PENALTY: f64 = 0.6;
/// Extra throughput discount: a browser is penalized more as the benchFit weight (wB) rises.
pub const BROWSER_THROUGHPUT_DISCOUNT: f64 = 0.3;
/// Boost for the payer's own node (zero network hop) when it is itself a candidate.
pub const PREFER_LOCAL_BOOST: f64 = 1.15;

/// Multiplicative score adjustment for a candidate's runtime kind + locality. Returns 1 for a native
/// remote node with no locality signal (i.e. no change). Pure.
///
/// `local_node_id` is the id whose candidacy earns the prefer-local boost — the JS engine's opt-in
/// `req.localNodeId`; the Rust blend passes the payer (its own node only enters the pool with
/// `allow_self`, which is exactly the opt-in).
pub fn runtime_kind_factor(candidate: &Candidate, local_node_id: Option<&str>, w: &Weights) -> f64 {
    let mut f = 1.0;
    let kind = candidate.profile.as_ref().and_then(|p| p.runtime.kind.as_deref());
    if kind == Some("Browser") {
        let w_b = if w.w_b.is_finite() { w.w_b } else { 0.0 };
        f *= BROWSER_RUNTIME_PENALTY * (1.0 - BROWSER_THROUGHPUT_DISCOUNT * clamp01(w_b));
    }
    if let Some(local) = local_node_id.filter(|l| !l.is_empty()) {
        if candidate.node_id == local {
            f *= PREFER_LOCAL_BOOST;
        }
    }
    f
}

// ----------------------------------------------------------------------------
// Tests — the JS scorer.__selftest fixtures translated per block.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        CpuProfile, GpuInfo, LlmInfo, MemoryInfo, NodeCapacity, NodeHistory, RuntimeInfo, StorageInfo,
    };

    fn cand(node_id: &str) -> Candidate {
        Candidate {
            node_id: node_id.into(),
            capacity: NodeCapacity { node_id: node_id.into(), ..Default::default() },
            profile: None,
            history: None,
            rtt_ms: f64::INFINITY,
            rtt_measured: false,
            ask_base_units: None,
            groups: None,
        }
    }

    /// The "profiled" fixture host from the JS selftest (24GB CUDA card, 120 tok/s).
    fn profiled(node_id: &str) -> Candidate {
        let mut c = cand(node_id);
        c.profile = Some(NodeProfile {
            node_id: node_id.into(),
            cpu: CpuProfile { cores: 8, threads: 16, gflops_fp32: 400.0, mem_bw_gbps: 50.0 },
            gpus: vec![GpuInfo { model: "X".into(), backend: "Cuda".into(), vram_mb: 24000.0, fp16_tflops: 80.0 }],
            memory: MemoryInfo { total_mb: 16000.0, available_mb: 12000.0 },
            storage: StorageInfo { total_gb: 1000.0, free_gb: 500.0, read_mbps: 2000.0, write_mbps: 1500.0 },
            llm: LlmInfo { ref_model: "m".into(), tokens_per_sec: 120.0 },
            runtime: RuntimeInfo { os: "linux".into(), arch: "x86_64".into(), docker: true, gvisor: true, wasm: true, kind: None },
            ..Default::default()
        });
        c
    }

    fn history(jobs: u64, heartbeats: u64, earned: &str, recent: &str) -> NodeHistory {
        NodeHistory {
            jobs_hosted: jobs,
            heartbeats_hosted: heartbeats,
            earned: earned.into(),
            recent_earned: recent.into(),
            ..Default::default()
        }
    }

    fn demand_vram(weight: f64, target: f64) -> Demand {
        Demand { vram_mb: Some(DemandAxis { weight, target }), ..Default::default() }
    }

    // --- resolve_weights (kept from the scaffold) ---------------------------

    #[test]
    fn clamp01_bounds() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(2.0), 1.0);
        assert_eq!(clamp01(0.5), 0.5);
        assert_eq!(clamp01(f64::NAN), 0.0);
    }

    #[test]
    fn resolve_weights_normalizes_convex_axes_and_keeps_wd() {
        let spec = JobSpec { objective: Some(Objective::Latency), ..Default::default() };
        let w = resolve_weights(&spec);
        let sum = w.w_l + w.w_b + w.w_t + w.w_p;
        assert!((sum - 1.0).abs() < 1e-9, "convex weights sum to 1, got {sum}");
        assert!(w.w_l > w.w_b && w.w_l > w.w_p, "latency objective weights latency highest");
        assert!((w.w_d - 0.1).abs() < 1e-9, "wD passes through verbatim, not normalized");
    }

    #[test]
    fn resolve_weights_default_balanced() {
        let w = resolve_weights(&JobSpec::default());
        let sum = w.w_l + w.w_b + w.w_t + w.w_p;
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn resolve_weights_override_renormalizes() {
        let spec = JobSpec {
            objective: Some(Objective::Balanced),
            weights: Some(Weights { w_l: 2.0, w_b: 2.0, w_t: 0.0, w_p: 0.0, w_d: 0.3 }),
            ..Default::default()
        };
        let w = resolve_weights(&spec);
        assert!((w.w_l - 0.5).abs() < 1e-9 && (w.w_b - 0.5).abs() < 1e-9, "override re-normalizes");
        assert_eq!(w.w_t, 0.0);
        assert!((w.w_d - 0.3).abs() < 1e-9, "wD override passes through");
    }

    #[test]
    fn resolve_weights_all_zero_falls_back_to_equal_split() {
        let spec = JobSpec {
            weights: Some(Weights { w_l: 0.0, w_b: 0.0, w_t: 0.0, w_p: 0.0, w_d: 0.15 }),
            ..Default::default()
        };
        let w = resolve_weights(&spec);
        assert_eq!(w.w_l, 0.25);
        assert_eq!(w.w_p, 0.25);
    }

    // --- latency: prefers low-RTT, floors past the cap ----------------------

    #[test]
    fn latency_prefers_low_rtt_and_floors_past_cap() {
        let spec = JobSpec { rtt_soft_cap_ms: Some(250.0), ..Default::default() };
        let mut near = cand("us-near");
        near.rtt_ms = 10.0;
        let mut far = cand("us-far");
        far.rtt_ms = 300.0;
        let s_near = latency_score(&near, &spec, None);
        let s_far = latency_score(&far, &spec, None);
        assert!((s_near - (1.0 - 10.0 / 250.0)).abs() < 1e-9, "near latency = 1-10/250, got {s_near}");
        assert!(s_near > s_far, "near host must outscore far host (prefers low-RTT)");
        assert_eq!(s_far, 0.0, "host past the soft cap floors at 0");
        // Unreachable / infinite RTT → 0.
        assert_eq!(latency_score(&cand("x"), &spec, None), 0.0, "unreachable host scores 0");
    }

    /// Graph-backed latency path (no candidate rtt_ms): the JS stub-graph block, via LatencyView.
    #[test]
    fn latency_graph_backed_branch() {
        struct Stub;
        impl LatencyView for Stub {
            fn measured_rtt(&self, a: &str, b: &str) -> Option<f64> {
                (a == "me" && b == "us-near").then_some(10.0)
            }
            fn predicted_rtt(&self, a: &str, b: &str) -> f64 {
                if a == "me" && b == "us-far" { 280.0 } else { f64::INFINITY }
            }
            fn region_of(&self, _: &str) -> i64 {
                -1
            }
        }
        let spec = JobSpec { payer: Some("me".into()), rtt_soft_cap_ms: Some(250.0), ..Default::default() };
        let s_near = latency_score(&cand("us-near"), &spec, Some(&Stub));
        assert!((s_near - (1.0 - 10.0 / 250.0)).abs() < 1e-9, "graph-backed measured RTT used for near host");
        assert_eq!(latency_score(&cand("us-far"), &spec, Some(&Stub)), 0.0, "predicted past the cap floors to 0");
        assert_eq!(latency_score(&cand("ghost"), &spec, Some(&Stub)), 0.0, "unreachable host scores 0");
    }

    // --- benchFit: saturates at target; atlas confidence discounted ---------

    #[test]
    fn bench_fit_saturates_at_target() {
        let host = profiled("gpu1");
        let spec = JobSpec { demand: Some(demand_vram(1.0, 8000.0)), ..Default::default() };
        let fit = bench_fit_score(&host, &spec);
        assert_eq!(fit.source, FitSource::Profile);
        assert_eq!(fit.confidence, 1.0);
        assert_eq!(fit.score, 1.0, "24GB VRAM clears an 8GB target => saturated fit 1.0");

        // Below the bar the fit is the ratio (no extra credit for surplus above it).
        let big = JobSpec { demand: Some(demand_vram(1.0, 48000.0)), ..Default::default() };
        let fit_big = bench_fit_score(&host, &big);
        assert!((fit_big.score - 24000.0 / 48000.0).abs() < 1e-9, "below-bar fit is the ratio (0.5 here)");

        // No demand => neutral 0.5, full confidence.
        let neutral = bench_fit_score(&host, &JobSpec::default());
        assert_eq!(neutral.score, 0.5);
        assert_eq!(neutral.confidence, 1.0);
    }

    #[test]
    fn bench_fit_atlas_fallback_is_discounted() {
        let mut atlas_only = cand("cpu1");
        atlas_only.capacity.cpu_cores = 16;
        atlas_only.capacity.mem_mb = 32000;
        atlas_only.capacity.tags = vec!["docker".into()];
        let spec = JobSpec {
            demand: Some(Demand { gflops: Some(DemandAxis { weight: 1.0, target: 64.0 }), ..Default::default() }),
            ..Default::default()
        };
        let fit = bench_fit_score(&atlas_only, &spec);
        assert_eq!(fit.source, FitSource::Atlas, "no profile => atlas source");
        assert!(fit.confidence < 1.0, "atlas estimate carries discounted confidence");
        assert_eq!(fit.score, 1.0, "16 cores * 8 GFLOPS = 128 clears a 64 target");
    }

    #[test]
    fn bench_fit_demand_weighted_mean() {
        let host = profiled("gpu1");
        let spec = JobSpec {
            demand: Some(Demand {
                vram_mb: Some(DemandAxis { weight: 3.0, target: 48000.0 }), // fit 0.5
                tokens_per_sec: Some(DemandAxis { weight: 1.0, target: 60.0 }), // fit 1.0 (120/60 saturates)
                ..Default::default()
            }),
            ..Default::default()
        };
        let fit = bench_fit_score(&host, &spec);
        let expected = (3.0 * 0.5 + 1.0 * 1.0) / 4.0;
        assert!((fit.score - expected).abs() < 1e-9, "demand-weighted mean = {expected}, got {}", fit.score);
    }

    // --- trust: log-saturating + §9 benchmark-suspect floor -----------------

    /// The H100-claim fixture host from the JS selftest (80GB VRAM, 200 TFLOPS, 300 tok/s).
    fn big_claim(node_id: &str) -> Candidate {
        let mut c = cand(node_id);
        c.profile = Some(NodeProfile {
            node_id: node_id.into(),
            cpu: CpuProfile { cores: 4, threads: 8, gflops_fp32: 100.0, mem_bw_gbps: 20.0 },
            gpus: vec![GpuInfo { model: "H100".into(), backend: "Cuda".into(), vram_mb: 80000.0, fp16_tflops: 200.0 }],
            memory: MemoryInfo { total_mb: 8000.0, available_mb: 6000.0 },
            storage: StorageInfo { total_gb: 100.0, free_gb: 50.0, read_mbps: 500.0, write_mbps: 400.0 },
            llm: LlmInfo { ref_model: "m".into(), tokens_per_sec: 300.0 },
            runtime: RuntimeInfo { os: "linux".into(), arch: "x86_64".into(), docker: true, ..Default::default() },
            ..Default::default()
        });
        c
    }

    #[test]
    fn trust_log_saturates_and_rewards_recency() {
        let spec = JobSpec { trust_saturation: Some(50.0), ..Default::default() };
        let mut newbie = cand("n");
        newbie.history = Some(history(0, 0, "0", "0"));
        let mut veteran = cand("v");
        veteran.history = Some(history(40, 10, "7200000000000000000000", "1200000000000000000000"));
        let t_new = trust_score(&newbie, &spec);
        let t_vet = trust_score(&veteran, &spec);
        assert!(t_new.score >= 0.0 && t_new.score < 0.2, "newbie trust must be low, got {}", t_new.score);
        assert!(t_vet.score > t_new.score, "veteran must outrank newbie on trust");
        assert!(!t_new.benchmark_suspect && !t_vet.benchmark_suspect, "honest histories are not suspect");

        // Log saturation: 500 delivered is only modestly above 50.
        let mut huge = cand("h");
        huge.history = Some(history(500, 0, "1", "1"));
        let mut fifty = cand("f");
        fifty.history = Some(history(50, 0, "1", "1"));
        let t_huge = trust_score(&huge, &spec);
        let t_fifty = trust_score(&fifty, &spec);
        assert!(t_huge.score - t_fifty.score < t_fifty.score, "trust saturates: 500 vs 50 gap < 50 vs 0 gap");

        // Recency discount: all-time earnings but nothing recent.
        let mut dormant = cand("d");
        dormant.history = Some(history(40, 10, "7200000000000000000000", "0"));
        let t_dormant = trust_score(&dormant, &spec);
        assert!(t_dormant.score < t_vet.score, "dormant (no recent earnings) trust < active veteran");
    }

    #[test]
    fn trust_flags_and_floors_benchmark_suspect() {
        let spec = JobSpec { trust_saturation: Some(50.0), ..Default::default() };

        // §9: big GPU/LLM claim with no delivered work => suspect + trust floored.
        let mut suspect = big_claim("s");
        suspect.history = Some(history(0, 0, "0", "0"));
        let t_sus = trust_score(&suspect, &spec);
        assert!(t_sus.benchmark_suspect, "huge claim + no delivered work => benchmark_suspect");
        assert_eq!(t_sus.score, 0.0, "suspect host with zero delivered work => trust floored to 0");

        // A node that has actually delivered the work for its big claim is NOT suspect.
        let mut earned = big_claim("e");
        earned.history = Some(history(30, 0, "1", "1"));
        let t_earned = trust_score(&earned, &spec);
        assert!(!t_earned.benchmark_suspect, "delivered work corroborates the claim => not suspect");
        assert!(t_earned.score > 0.0, "corroborated big-claim host keeps positive trust");

        // No history at all => low trust, not suspect (no profile => nothing to over-claim).
        let empty = trust_score(&cand("e2"), &spec);
        assert_eq!(empty.score, 0.0);
        assert!(!empty.benchmark_suspect, "no history => zero trust, not suspect");
    }

    // --- price: 128-bit money, ratio-only float, monotonic ------------------

    #[test]
    fn price_is_integer_money_and_monotonic() {
        // Base units beyond 2^53 to prove no float coercion of money.
        let cap_units = "1000000000000000000000"; // 1000 credits
        let spec = JobSpec {
            price_cap_base_units: Some(cap_units.into()),
            default_price_score: Some(0.5),
            ..Default::default()
        };
        let mut cheap = cand("c");
        cheap.ask_base_units = Some("100000000000000000000".into()); // 100 credits
        let mut dear = cand("d");
        dear.ask_base_units = Some("900000000000000000000".into()); // 900 credits
        let s_cheap = price_score(&cheap, &spec);
        let s_dear = price_score(&dear, &spec);
        assert!(s_cheap > s_dear, "cheaper must score higher (monotonic)");
        assert!((s_cheap - 0.9).abs() < 1e-6, "100/1000 => 0.9, got {s_cheap}");
        assert!((s_dear - 0.1).abs() < 1e-6, "900/1000 => 0.1, got {s_dear}");

        // At/over the cap => 0; free => 1.
        let mut at_cap = cand("a");
        at_cap.ask_base_units = Some(cap_units.into());
        assert_eq!(price_score(&at_cap, &spec), 0.0, "at the cap => 0");
        let mut free = cand("f");
        free.ask_base_units = Some("0".into());
        assert_eq!(price_score(&free, &spec), 1.0, "free => 1");

        // No ask => default neutral; no cap => default neutral.
        assert_eq!(price_score(&cand("n"), &spec), 0.5, "no ask => default 0.5");
        let mut asks = cand("x");
        asks.ask_base_units = Some("5".into());
        let no_cap = JobSpec { default_price_score: Some(0.5), ..Default::default() };
        assert_eq!(price_score(&asks, &no_cap), 0.5, "no cap => default 0.5");

        // Malformed money strings degrade to neutral, never panic.
        let mut bad = cand("b");
        bad.ask_base_units = Some("not-a-number".into());
        assert_eq!(price_score(&bad, &spec), 0.5, "malformed ask => neutral");
    }

    // --- runtime kind factor + prefer-local ----------------------------------

    #[test]
    fn runtime_kind_factor_penalizes_browsers_and_boosts_local() {
        let with_kind = |id: &str, kind: &str| {
            let mut c = cand(id);
            c.profile = Some(NodeProfile {
                runtime: RuntimeInfo { kind: Some(kind.into()), ..Default::default() },
                ..Default::default()
            });
            c
        };
        let w_tp = resolve_weights(&JobSpec { objective: Some(Objective::Throughput), ..Default::default() });
        assert_eq!(runtime_kind_factor(&with_kind("n", "Native"), None, &w_tp), 1.0, "native gets no adjustment");

        let browser = runtime_kind_factor(&with_kind("b", "Browser"), None, &w_tp);
        assert!(browser < 1.0, "browser node is penalized");
        assert!(browser <= BROWSER_RUNTIME_PENALTY, "at least the base penalty on throughput");

        // The browser penalty is harsher on throughput than on latency objectives.
        let w_lat = resolve_weights(&JobSpec { objective: Some(Objective::Latency), ..Default::default() });
        let browser_lat = runtime_kind_factor(&with_kind("b", "Browser"), None, &w_lat);
        assert!(browser_lat > browser, "browser penalized harder when throughput-bound (high wB)");

        // No profile -> unknown kind -> no penalty.
        assert_eq!(runtime_kind_factor(&cand("x"), None, &w_tp), 1.0, "missing profile -> no runtime penalty");

        // Prefer-local: only the matching node is boosted.
        let local = runtime_kind_factor(&with_kind("me", "Native"), Some("me"), &w_tp);
        assert!((local - PREFER_LOCAL_BOOST).abs() < 1e-9, "payer's own node gets the prefer-local boost");
        assert_eq!(runtime_kind_factor(&with_kind("other", "Native"), Some("me"), &w_tp), 1.0);
    }

    // --- static blend: convex, monotone, near/honest beats far/suspect ------

    #[test]
    fn static_score_ranks_near_honest_above_far_suspect() {
        let spec = JobSpec {
            objective: Some(Objective::Balanced),
            payer: Some("me".into()),
            rtt_soft_cap_ms: Some(250.0),
            trust_saturation: Some(50.0),
            demand: Some(demand_vram(1.0, 8000.0)),
            ..Default::default()
        };

        let mut good = profiled("us-near");
        good.rtt_ms = 10.0;
        good.capacity.tags = vec!["docker".into(), "gpu".into()];
        good.history = Some(history(40, 10, "7200000000000000000000", "1200000000000000000000"));

        let mut bad = big_claim("us-far");
        bad.rtt_ms = 300.0; // past the cap => latency 0
        bad.capacity.tags = vec!["docker".into(), "gpu".into()];
        bad.history = Some(history(0, 0, "0", "0")); // no delivered work

        let s_good = static_score(&good, &spec, None);
        let s_bad = static_score(&bad, &spec, None);

        assert!(s_good.score >= 0.0 && s_good.score <= 1.0, "static score in [0,1]");
        assert!(s_bad.score >= 0.0 && s_bad.score <= 1.0, "static score in [0,1]");
        assert!(s_good.score > s_bad.score, "near/honest host must outrank far/benchmark-suspect host");
        assert!(s_bad.benchmark_suspect, "the over-claiming far host is flagged suspect in the blend");
        assert_eq!(s_good.bench_fit.source, FitSource::Profile);
        assert_eq!(s_good.bench_fit.confidence, 1.0);
        // The far host scores 0 on latency; the near host gets the full latency contribution.
        assert!(s_good.parts.latency > 0.0);
        assert_eq!(s_bad.parts.latency, 0.0, "latency part reflects RTT (low-RTT preferred)");
    }

    /// Blend monotonicity: improving any single axis (all else equal) never lowers the blend.
    #[test]
    fn static_score_is_monotone_per_axis() {
        let spec = JobSpec {
            payer: Some("me".into()),
            price_cap_base_units: Some("1000".into()),
            demand: Some(demand_vram(1.0, 48000.0)),
            ..Default::default()
        };
        // Enough delivered work (30 >= the largest §9 corroboration floor of 20) that the
        // benchmark-suspect check never fires — this test isolates pure per-axis monotonicity.
        // (Raising a CLAIM without the work to back it can legitimately LOWER the blend; that
        // §9 non-monotonicity is covered by trust_flags_and_floors_benchmark_suspect.)
        let base = {
            let mut c = profiled("a");
            c.rtt_ms = 100.0;
            c.ask_base_units = Some("500".into());
            c.history = Some(history(30, 0, "1", "1"));
            c
        };
        let score = |c: &Candidate| static_score(c, &spec, None).score;

        let mut nearer = base.clone();
        nearer.rtt_ms = 10.0;
        assert!(score(&nearer) > score(&base), "lower RTT never lowers the blend");

        let mut cheaper = base.clone();
        cheaper.ask_base_units = Some("100".into());
        assert!(score(&cheaper) > score(&base), "cheaper ask never lowers the blend");

        let mut trusted = base.clone();
        trusted.history = Some(history(200, 0, "1", "1"));
        assert!(score(&trusted) > score(&base), "more delivered work never lowers the blend");

        let mut fitter = base.clone();
        fitter.profile.as_mut().unwrap().gpus[0].vram_mb = 48000.0;
        assert!(score(&fitter) > score(&base), "a better bench fit never lowers the blend");

        // A browser runtime strictly lowers an otherwise-identical candidate.
        let mut browser = base.clone();
        browser.profile.as_mut().unwrap().runtime.kind = Some("Browser".into());
        assert!(score(&browser) < score(&base), "browser candidate ranks below identical native one");
    }
}
