//! The selector — the pure placement brain: `(JobSpec, FabricMap) -> PlacementPlan`.
//!
//! Rust port of the JS `src/placer.js`. Unlike the JS version (whose `plan()` also did the I/O), the
//! Rust core planner is **fully pure**: the daemon ([`ce-sched-daemon`]) gathers the [`FabricMap`]
//! (from the local ce-bench daemon and/or the node read-substrate, including the `/beacon` reference
//! and per-node `/history` facts) and hands it to [`plan`]; the planner never touches a node. This
//! is the "thin library + daemon" refinement from `../ce-bench/docs/RUST-DAEMON-ARCH.md`: the SDK
//! can call [`plan`] directly with no daemon hop.
//!
//! Pipeline (mirrors the JS module + `docs/placement-design.md`):
//!
//! - [`plan`] — build graph → [`feasible`] → [`vendor::tag_candidates`](crate::vendor::tag_candidates)
//!   → [`beacon_seed`] → [`select`] → assemble.
//! - [`feasible`] — §1 hard filter (liveness/headroom/tags/floor/reachability/self) over the map.
//! - [`redundancy_for`] — §3 effectiveK.
//! - [`select`] / [`select_with`] — §5–§7 constraint-satisfaction (greedy live re-rank, hard caps,
//!   graded relaxation, beacon tie-break or softmax, cohort pass).
//! - [`beacon_seed`] — §6 deterministic PRNG seed (FNV-1a over beacon + request identity), feeding
//!   the same [`Mulberry32`](crate::graph::Mulberry32) stream as the JS engine, bit-for-bit.
//!
//! The selection is pure given a beacon-derived `seed`, so the whole policy is replayable:
//! `(beacon, request, candidate set) -> identical plan` — the auditability guarantee from §6.
//! Determinism note: where the JS `plan()` read the wall clock (`Date.now()`), the pure port derives
//! "now" and the plan's `assembled_at_ms` from `map.assembled_at_ms`, so `(spec, map)` fully
//! determines the plan.

use crate::api::{
    BeaconRef, Cohort, FabricMap, Groups, JobSpec, NodeCapacity, NodeHistory, NodeProfile,
    PlacementPlan, PlanTarget, Redundancy, RedundancyPolicy, RejectedHost, Selection,
};
use crate::graph::{build_graph, GraphOptions, LatencyView, Mulberry32};
use crate::scorer::{clamp01, resolve_weights, static_score, StaticScore};
use crate::vendor::{diversity_penalty, per_group_cap, tag_candidates, GroupKey, UNKNOWN};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

/// Why the pure planner could not produce a plan. (The daemon maps these to HTTP statuses.)
/// An over-constrained-but-valid request is NOT an error — it yields an empty/short plan with a
/// `shortfall`, exactly like the JS engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// `spec.payer` was not set and the caller did not resolve it (the daemon fills it from `/status`).
    MissingPayer,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::MissingPayer => {
                write!(f, "plan: spec.payer (the latency origin node id) is required")
            }
        }
    }
}

impl Error for PlanError {}

/// A feasibility-filtered placement candidate. Built by [`feasible`], enriched with `.groups` by
/// [`vendor::tag_candidates`](crate::vendor::tag_candidates), scored by the scorer. The Rust
/// equivalent of the JS `Candidate` typedef.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub node_id: String,
    pub capacity: NodeCapacity,
    /// `None` => atlas fallback in use (benchFit estimates, discounted trust confidence).
    pub profile: Option<NodeProfile>,
    /// Raw `/history` reputation facts, when the map carried them.
    pub history: Option<NodeHistory>,
    /// Measured-or-predicted RTT to the payer (ms).
    pub rtt_ms: f64,
    /// True if `rtt_ms` is a direct measured sample (ground truth).
    pub rtt_measured: bool,
    /// Advertised price (base-unit string), if any.
    pub ask_base_units: Option<String>,
    /// Vendor grouping keys, set by `tag_candidates`.
    pub groups: Option<Groups>,
}

/// A scoring function: maps a candidate + request + graph to a static score breakdown. The placer
/// calls it once per candidate. The default is [`scorer::static_score`](crate::scorer::static_score);
/// tests inject a synthetic one via [`select_with`].
pub type ScoreFn<'a> = dyn Fn(&Candidate, &JobSpec, Option<&dyn LatencyView>) -> StaticScore + 'a;

// ----------------------------------------------------------------------------
// Resolved request defaults (the JS REQUEST_DEFAULTS / withDefaults, as accessors).
// ----------------------------------------------------------------------------

fn spec_k(spec: &JobSpec) -> u32 {
    spec.k.filter(|&k| k > 0).unwrap_or(1)
}

fn spec_max_stale_secs(spec: &JobSpec) -> u64 {
    spec.max_stale_secs.unwrap_or(180)
}

fn spec_max_share(spec: &JobSpec) -> f64 {
    spec.max_share.unwrap_or(0.34)
}

fn spec_redundancy(spec: &JobSpec) -> Redundancy {
    spec.redundancy.unwrap_or_default()
}

fn spec_cohort(spec: &JobSpec) -> Cohort {
    spec.cohort.unwrap_or_default()
}

fn spec_selection(spec: &JobSpec) -> Selection {
    spec.selection.unwrap_or_default()
}

/// The tie window for the beacon break (§6). JS accepted any finite value (even negative) here.
fn spec_tie_eps(spec: &JobSpec) -> f64 {
    spec.tie_eps.filter(|e| e.is_finite()).unwrap_or(0.02)
}

// ----------------------------------------------------------------------------
// Beacon-seeded PRNG (§6).
// ----------------------------------------------------------------------------

/// Fold a string into a 32-bit hash (FNV-1a) over UTF-16 code units — bit-for-bit the JS
/// `charCodeAt` loop. Used to mix the beacon hash + request identity into the PRNG seed so it is
/// unpredictable before dispatch yet replayable after.
fn hash32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for unit in s.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// §6 deterministic PRNG seed from the beacon mixed with the request identity (nonce/payer) so the
/// seed is unpredictable before dispatch yet auditable after: anyone can replay
/// `(beacon, request, candidate set) -> same plan`. Pure; reproduces the JS `beaconSeed` exactly
/// (the JS `>>> 0` height coercion is ToUint32 = truncation to `u32`).
pub fn beacon_seed(beacon: &BeaconRef, spec: &JobSpec) -> u32 {
    let height = beacon.height as u32;
    let nonce = spec.nonce.as_deref().unwrap_or("");
    let payer = spec.payer.as_deref().unwrap_or("");
    // Mix all four identity components. XOR-fold so the order of contributions is irrelevant and
    // every component perturbs the whole 32-bit space.
    let mut seed = height;
    seed ^= hash32(&beacon.hash);
    seed ^= hash32(payer).wrapping_mul(0x9e37_79b1);
    seed ^= hash32(nonce).wrapping_mul(0x85eb_ca6b);
    // Guarantee a non-zero seed (mulberry32 with 0 still streams, but keep it deterministic + lively).
    if seed == 0 { 0x6d2b_79f5 } else { seed }
}

// ----------------------------------------------------------------------------
// §1 — candidate build (hard feasibility filter).
// ----------------------------------------------------------------------------

/// Estimated CPU cores already committed on a host. With no per-job accounting on the wire we charge
/// a conservative one core per running job (the design's §1.2 fallback heuristic).
fn estimated_used_cores(cap: &NodeCapacity) -> u32 {
    cap.running_jobs
}

/// Available memory (MB) for a candidate. With a NodeProfile, a positive `memory.available_mb` is
/// authoritative; otherwise fall back to advertised `mem_mb` minus a per-running-job reservation.
/// (The JS checked "field present"; on the typed wire an absent field decodes as 0, so "positive"
/// is the presence test here.)
fn available_mem_mb(cap: &NodeCapacity, profile: Option<&NodeProfile>) -> f64 {
    if let Some(p) = profile {
        if p.memory.available_mb.is_finite() && p.memory.available_mb > 0.0 {
            return p.memory.available_mb;
        }
    }
    // Coarse fallback: assume each running job holds ~512 MB. Never negative.
    (cap.mem_mb as f64 - cap.running_jobs as f64 * 512.0).max(0.0)
}

/// Read a profile axis (VRAM / GFLOPS / tokens-per-sec) for the §1.4 capability floor. `None` when
/// the axis is not measured (no profile / no GPUs).
fn floor_axis(profile: &NodeProfile, axis: &str) -> Option<f64> {
    match axis {
        "gflops" => Some(profile.cpu.gflops_fp32),
        "tokens" => Some(profile.llm.tokens_per_sec),
        "vram" => {
            if profile.gpus.is_empty() {
                return None;
            }
            Some(profile.gpus.iter().map(|g| if g.vram_mb.is_finite() { g.vram_mb } else { 0.0 }).fold(0.0, f64::max))
        }
        _ => None,
    }
}

/// §1 hard feasibility filter: [`FabricMap`] rows -> [`Candidate`]s. Pure. Each survivor is a
/// fully-formed candidate (capacity, profile/history when the map carried them, measured/predicted
/// RTT to the payer, advertised ask if any). Hard constraints are pass/fail and never trade off
/// against score:
///   1. liveness            `now - last_seen_secs <= max_stale_secs`
///   2. resource headroom   free cores >= cpu_cores AND available mem >= mem_mb
///   3. required tags       every `spec.require_tags` present
///   4. capability floor    min_gflops/min_vram_mb/min_tokens_per_sec met by a measured profile;
///                          no profile + a floor => excluded iff `spec.require_profile`
///   5. reachability        finite predicted RTT from the payer
///   6. self / exclusion    not in `spec.exclude`, and (unless `allow_self`) not the payer itself
pub fn feasible(map: &FabricMap, spec: &JobSpec, graph: &dyn LatencyView, now: u64) -> Vec<Candidate> {
    let payer = spec.payer.as_deref().unwrap_or("");
    let exclude: HashSet<&str> = spec.exclude.iter().map(String::as_str).collect();
    let max_stale = spec_max_stale_secs(spec);
    let finite = |x: Option<f64>| x.filter(|v| v.is_finite());
    let (min_gflops, min_vram, min_tokens) =
        (finite(spec.min_gflops), finite(spec.min_vram_mb), finite(spec.min_tokens_per_sec));
    let has_floor = min_gflops.is_some() || min_vram.is_some() || min_tokens.is_some();

    let mut out = Vec::new();
    for node in &map.nodes {
        let node_id = node.node_id.as_str();
        if node_id.is_empty() {
            continue;
        }

        // 6. self / exclusion (cheapest, do first).
        if exclude.contains(node_id) {
            continue;
        }
        if !spec.allow_self && node_id == payer {
            continue;
        }

        let cap = &node.capacity;
        let profile = node.profile.as_ref();

        // 1. liveness (a clock-skewed future last_seen passes, like the JS).
        let age = now as i128 - cap.last_seen_secs as i128;
        if age > max_stale as i128 {
            continue;
        }

        // 2. resource headroom.
        let free_cores = cap.cpu_cores as i64 - estimated_used_cores(cap) as i64;
        if free_cores < spec.cpu_cores as i64 {
            continue;
        }
        if available_mem_mb(cap, profile) < spec.mem_mb as f64 {
            continue;
        }

        // 3. required tags.
        if !spec.require_tags.iter().all(|t| cap.tags.contains(t)) {
            continue;
        }

        // 4. capability floor. If a floor is declared and there is no measured profile, admit only
        //    when require_profile is false (unverified hardware — discounted later by trust, not
        //    excluded here).
        if has_floor {
            match profile {
                None => {
                    if spec.require_profile {
                        continue;
                    }
                }
                Some(p) => {
                    let below = |min: Option<f64>, axis: &str| {
                        min.is_some_and(|m| floor_axis(p, axis).is_none_or(|v| v < m))
                    };
                    if below(min_gflops, "gflops") || below(min_vram, "vram") || below(min_tokens, "tokens") {
                        continue;
                    }
                }
            }
        }

        // 5. reachability — an unreachable host cannot serve. Measured RTT is ground truth when present.
        let measured = graph.measured_rtt(payer, node_id);
        let rtt_measured = measured.is_some();
        let rtt_ms = measured.unwrap_or_else(|| graph.predicted_rtt(payer, node_id));
        if !rtt_ms.is_finite() {
            continue;
        }

        out.push(Candidate {
            node_id: node_id.to_string(),
            capacity: cap.clone(),
            profile: node.profile.clone(),
            history: node.history.clone(),
            rtt_ms,
            rtt_measured,
            ask_base_units: cap.ask_base_units.clone(),
            groups: None,
        });
    }
    out
}

// ----------------------------------------------------------------------------
// §3 — redundancy factor (how many hosts).
// ----------------------------------------------------------------------------

/// Coarse trust estimate of a candidate from its `/history` facts, in `[0,1]`, used ONLY to decide
/// the replication count (the real, weighted trust score is the scorer's job during selection).
/// Reads the delivered-work count (jobs + heartbeats hosted) on a log-saturating curve. No
/// profile/history => 0.
fn coarse_trust(c: &Candidate, spec: &JobSpec) -> f64 {
    let Some(h) = &c.history else {
        return if c.profile.is_some() { 0.05 } else { 0.0 }; // a profile alone is weak signal
    };
    let delivered = (h.jobs_hosted + h.heartbeats_hosted) as f64;
    let sat = spec.trust_saturation.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(50.0);
    if delivered <= 0.0 {
        return 0.0;
    }
    clamp01(delivered.ln_1p() / sat.ln_1p())
}

/// §3 effectiveK: max(`spec.k`, replication implied by the best feasible trust + `spec.redundancy`).
/// Pure.
///
/// - `"none"`          -> `spec.k` (trust the single best).
/// - `"verify"`        -> at least 3 INDEPENDENT replicas; if the best reachable host is already
///                        high-trust, 2 suffice (still cross-checked), but never < 2.
/// - confidence `(0,1)`-> map the best feasible trust to the replica count needed to reach the
///                        target confidence. High-trust hosts => fewer.
///
/// The replication count is bounded by the candidate pool size (cannot place more than exist).
pub fn redundancy_for(candidates: &[Candidate], spec: &JobSpec) -> u32 {
    let base_k = spec_k(spec);
    let pool_size = candidates.len() as u32;

    // Best feasible trust drives how much redundancy a policy actually demands.
    let mut best_trust: f64 = 0.0;
    for c in candidates {
        best_trust = best_trust.max(coarse_trust(c, spec));
    }

    let mut replication = base_k;
    match spec_redundancy(spec) {
        Redundancy::Policy(RedundancyPolicy::Verify) => {
            // Verification needs independent replicas to majority-vote. High-trust host => 2 (still
            // compared); anything less => 3.
            replication = if best_trust >= 0.6 { 2 } else { 3 };
        }
        Redundancy::Confidence(target) if target.is_finite() && target > 0.0 && target < 1.0 => {
            // Target confidence. Per-host success prob ~ 0.5 + 0.49*bestTrust (an unverified host is
            // a coin flip; a fully-trusted host nearly always delivers). Replicas needed so
            // 1-(1-p)^n >= target.
            let p = (0.5 + 0.49 * best_trust).min(0.99);
            let mut n = 1u32;
            while 1.0 - (1.0 - p).powi(n as i32) < target && n < 16 {
                n += 1;
            }
            replication = n;
        }
        // "none" (and out-of-range confidences, as in JS) leave replication = base_k.
        _ => {}
    }

    let effective = base_k.max(replication);
    // Never demand more replicas than the pool can supply (a SHORT plan is reported via shortfall by
    // select(), but effectiveK itself should not exceed what is even theoretically placeable).
    if pool_size > 0 { effective.min(base_k.max(pool_size)) } else { effective }
}

// ----------------------------------------------------------------------------
// §5–§7 — constraint-satisfaction selection.
// ----------------------------------------------------------------------------

/// Cohort co-location adjustment (§7) to a candidate's live score given what is already chosen:
///   - `colocate`: reward low predicted RTT to the chosen members (chatty / tensor-parallel jobs) —
///     a bonus in `[0, 0.15]` that shrinks with mean inter-member RTT.
///   - `spread`:   reward landing in a NOT-yet-used region (availability under a regional outage).
///   - `dag`:      documented extension; v0 treats it as `spread`.
/// Returns a signed delta added to the live score. Pure.
fn cohort_adjust(c: &Candidate, chosen: &[&Candidate], spec: &JobSpec, graph: Option<&dyn LatencyView>) -> f64 {
    if chosen.is_empty() {
        return 0.0;
    }
    if spec_cohort(spec) == Cohort::Colocate {
        let Some(graph) = graph else { return 0.0 };
        let mut sum = 0.0;
        let mut n = 0u32;
        for ch in chosen {
            let rtt = graph.predicted_rtt(&c.node_id, &ch.node_id);
            if rtt.is_finite() {
                sum += rtt;
                n += 1;
            }
        }
        if n == 0 {
            return 0.0;
        }
        let mean_rtt = sum / n as f64;
        let cap = spec.rtt_soft_cap_ms.filter(|x| x.is_finite() && *x > 0.0).unwrap_or(250.0);
        let coloc_bonus_max = 0.15;
        // Closer to the cohort => larger bonus; flattens to 0 past the soft cap.
        return coloc_bonus_max * clamp01(1.0 - mean_rtt / cap);
    }
    // "spread" / "dag": small bonus for a fresh region; the diversity penalty already does the heavy work.
    let Some(c_reg) = c.groups.as_ref().map(|g| g.region.as_str()) else { return 0.0 };
    if c_reg.is_empty() || c_reg.starts_with("r?:") {
        return 0.0; // region-less node: no spread signal
    }
    for ch in chosen {
        if ch.groups.as_ref().is_some_and(|g| g.region == c_reg) {
            return 0.0; // region already used → no bonus
        }
    }
    0.05 // fresh region bonus
}

/// Softmax-sample an index from `live` scores using the beacon-seeded PRNG (§6 stochastic selection).
fn softmax_sample(live: &[(usize, f64)], temperature: Option<f64>, rand: &mut Mulberry32) -> usize {
    let t = temperature.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(0.15);
    let mut max = f64::NEG_INFINITY;
    for &(_, l) in live {
        if l > max {
            max = l;
        }
    }
    let weights: Vec<f64> = live.iter().map(|&(_, l)| ((l - max) / t).exp()).collect();
    let total: f64 = weights.iter().sum();
    if !(total > 0.0) {
        return 0;
    }
    let mut x = rand.next() * total;
    for (i, w) in weights.iter().enumerate() {
        x -= w;
        if x <= 0.0 {
            return i;
        }
    }
    weights.len() - 1
}

/// Per-key hard cap gate honoring a relaxed-group set: a group key in `relaxed_set` is not enforced.
/// The distinct-operator gate is always enforced when `require_distinct_operator`. Mirrors the JS
/// `capGate` + `enforcedOffender` pair (whose combined effect is exactly this manual per-key scan —
/// the JS bridged through `vendor.violatesCap` and then re-derived per-key anyway).
fn cap_gate(
    c: &Candidate,
    chosen: &[&Candidate],
    per_group: u32,
    require_distinct_operator: bool,
    relaxed_set: &HashSet<&'static str>,
) -> Option<GroupKey> {
    let Some(cg) = &c.groups else { return None };
    let mut op = 0u32;
    let mut asn = 0u32;
    let mut region = 0u32;
    let mut cluster = 0u32;
    for ch in chosen {
        let Some(g) = &ch.groups else { continue };
        if g.operator == cg.operator {
            op += 1;
        }
        if cg.asn != UNKNOWN && g.asn == cg.asn {
            asn += 1;
        }
        if !cg.region.starts_with("r?:") && g.region == cg.region {
            region += 1;
        }
        if g.cluster == cg.cluster {
            cluster += 1;
        }
    }
    if require_distinct_operator && op > 0 {
        return Some(GroupKey::Operator);
    }
    if !relaxed_set.contains("operator") && op + 1 > per_group {
        return Some(GroupKey::Operator);
    }
    if !relaxed_set.contains("asn") && cg.asn != UNKNOWN && asn + 1 > per_group {
        return Some(GroupKey::Asn);
    }
    if !relaxed_set.contains("region") && !cg.region.starts_with("r?:") && region + 1 > per_group {
        return Some(GroupKey::Region);
    }
    if !relaxed_set.contains("cluster") && cluster + 1 > per_group {
        return Some(GroupKey::Cluster);
    }
    None
}

/// What [`select`] hands back to [`plan`]: the chosen targets plus full provenance.
#[derive(Debug, Clone)]
pub struct SelectOutcome {
    pub targets: Vec<PlanTarget>,
    /// Independence relaxations applied, in ladder order (`region`/`asn`/`operator`).
    pub relaxed: Vec<String>,
    /// Missing replicas (0 if fully satisfied).
    pub shortfall: u32,
    pub rejected: Vec<RejectedHost>,
    /// The §3 replication the selection aimed for.
    pub effective_k: u32,
}

/// §5–§7 constraint-satisfaction selection with the default scorer
/// ([`scorer::static_score`](crate::scorer::static_score)). See [`select_with`].
pub fn select(candidates: Vec<Candidate>, spec: &JobSpec, graph: Option<&dyn LatencyView>, seed: u32) -> SelectOutcome {
    select_with(candidates, spec, graph, seed, &|c, s, g| static_score(c, s, g))
}

/// §5–§7 constraint-satisfaction selection. Pure given `seed` (beacon-derived) and `score_fn`.
/// Greedy with live re-ranking, hard group/operator caps, graded relaxation
/// (region → asn → operator; operator never relaxes under `verify`, region always relaxes under
/// `colocate`), beacon tie-break or softmax, and the cohort pass. Returns the chosen targets plus
/// full provenance for the plan.
///
/// `candidates` must be feasibility-filtered + vendor-tagged (carry `.groups`).
pub fn select_with(
    candidates: Vec<Candidate>,
    spec: &JobSpec,
    graph: Option<&dyn LatencyView>,
    seed: u32,
    score_fn: &ScoreFn<'_>,
) -> SelectOutcome {
    let pool = candidates;
    let mut rand = Mulberry32::new(seed);

    // Resolve blend weights once (for wD and the cohort/score math).
    let weights = resolve_weights(spec);
    let w_d = if weights.w_d.is_finite() { weights.w_d } else { 0.15 };

    let effective_k = redundancy_for(&pool, spec);
    let require_distinct_operator = spec_redundancy(spec) == Redundancy::Policy(RedundancyPolicy::Verify);

    // Pre-score every candidate once (static; the contextual penalty is applied live).
    let scored: Vec<StaticScore> = pool.iter().map(|c| score_fn(c, spec, graph)).collect();

    let mut chosen: Vec<usize> = Vec::new();
    let mut chosen_ids: HashSet<String> = HashSet::new();
    let mut relaxed: Vec<String> = Vec::new();
    let mut rejected_reason: HashMap<String, &'static str> = HashMap::new();

    let base_cap = per_group_cap(effective_k, spec_max_share(spec));

    // Relaxation ladder: each entry loosens ONE soft cap, region -> asn -> operator, recording each
    // compromise. Under "verify" the operator cap is NEVER relaxed (cross-operator independence is
    // the point); under "colocate" the region cap is relaxed by design. Step -1 = no relaxation.
    let ladder: [&'static str; 3] = ["region", "asn", "operator"];

    while chosen.len() < effective_k as usize && pool.len() > chosen.len() {
        let mut picked: Option<usize> = None;
        let mut relax_step: i32 = -1;

        // Try with progressively relaxed caps until something is admissible.
        for step in -1i32..ladder.len() as i32 {
            let mut relaxed_set: HashSet<&'static str> =
                ladder[..(step + 1) as usize].iter().copied().collect();
            if require_distinct_operator {
                relaxed_set.remove("operator");
            }
            if spec_cohort(spec) == Cohort::Colocate {
                relaxed_set.insert("region");
            }

            let chosen_refs: Vec<&Candidate> = chosen.iter().map(|&i| &pool[i]).collect();
            let mut live: Vec<(usize, f64)> = Vec::new();
            for (i, c) in pool.iter().enumerate() {
                if chosen_ids.contains(&c.node_id) {
                    continue;
                }
                if let Some(offending) = cap_gate(c, &chosen_refs, base_cap, require_distinct_operator, &relaxed_set) {
                    // Only record the FIRST (un-relaxed) reason so rejected[] reflects the real cause.
                    if step == -1 && !rejected_reason.contains_key(&c.node_id) {
                        rejected_reason.insert(
                            c.node_id.clone(),
                            if offending == GroupKey::Operator { "operator_dup" } else { "group_cap" },
                        );
                    }
                    continue;
                }
                let penalty = w_d * diversity_penalty(c, &chosen_refs, spec);
                let cohort = cohort_adjust(c, &chosen_refs, spec, graph);
                live.push((i, scored[i].score - penalty + cohort));
            }

            if live.is_empty() {
                continue; // nothing admissible at this relaxation level; loosen further
            }

            live.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| pool[a.0].node_id.cmp(&pool[b.0].node_id))
            });

            if spec_selection(spec) == Selection::Weighted {
                let idx = softmax_sample(&live, spec.temperature, &mut rand);
                picked = Some(live[idx].0);
            } else {
                // best + beacon tie-break among near-ties within tie_eps.
                let top = live[0].1;
                let eps = spec_tie_eps(spec);
                let ties: Vec<&(usize, f64)> = live.iter().filter(|x| top - x.1 <= eps).collect();
                picked = if ties.len() > 1 {
                    Some(ties[(rand.next() * ties.len() as f64).floor() as usize].0)
                } else {
                    Some(live[0].0)
                };
            }
            relax_step = step;
            break;
        }

        let Some(picked) = picked else { break }; // even fully-relaxed nothing fits — short plan.

        // Record any relaxation actually used for this pick.
        if relax_step >= 0 {
            for key in &ladder[..(relax_step + 1) as usize] {
                if *key == "operator" && require_distinct_operator {
                    continue;
                }
                if !relaxed.iter().any(|r| r == key) {
                    relaxed.push(key.to_string());
                }
            }
        }

        chosen_ids.insert(pool[picked].node_id.clone());
        chosen.push(picked);
        rejected_reason.remove(&pool[picked].node_id); // it was chosen, not rejected
    }

    let shortfall = effective_k.saturating_sub(chosen.len() as u32);

    // Anything feasible-but-unchosen with no recorded cap reason lost on score.
    let mut rejected: Vec<RejectedHost> = Vec::new();
    for (i, c) in pool.iter().enumerate() {
        if chosen_ids.contains(&c.node_id) {
            continue;
        }
        let reason = rejected_reason.get(&c.node_id).copied().unwrap_or(if scored[i].benchmark_suspect {
            "benchmark_suspect"
        } else {
            "low_score"
        });
        rejected.push(RejectedHost { node_id: c.node_id.clone(), reason: reason.to_string() });
    }

    let targets: Vec<PlanTarget> = chosen
        .iter()
        .enumerate()
        .map(|(replica, &i)| {
            let c = &pool[i];
            let s = &scored[i];
            PlanTarget {
                node_id: c.node_id.clone(),
                score: s.score,
                rtt_ms: c.rtt_ms,
                bench_fit: s.parts.bench_fit,
                trust: s.parts.trust,
                groups: c.groups.clone().unwrap_or_else(|| Groups {
                    operator: c.node_id.clone(),
                    asn: UNKNOWN.to_string(),
                    region: "r?".to_string(),
                    cluster: c.node_id.clone(),
                }),
                replica: replica as u32,
            }
        })
        .collect();

    SelectOutcome { targets, relaxed, shortfall, rejected, effective_k }
}

// ----------------------------------------------------------------------------
// plan() — the pure orchestrator.
// ----------------------------------------------------------------------------

/// Produce a [`PlacementPlan`] for `spec` against the assembled `map`. Pure and deterministic:
/// the beacon reference travels inside the map and "now" is the map's assembly time, so
/// `(spec, map) -> identical plan`, replayable by anyone with the same inputs (§6).
///
/// Pipeline (the JS `plan()` with the I/O hoisted into the map):
///   1. graph = build_graph(map)                        // latency substrate, payer-anchored queries
///   2. cand0 = feasible(map, spec, graph, now)         // §1 hard filter (history rides on the map)
///   3. cand  = vendor::tag_candidates(cand0, graph)    // vendor groups
///   4. seed  = beacon_seed(map.beacon, spec)           // §6
///   5. select(cand, spec, graph, seed)                 // §5–§7
///   6. assemble the auditable PlacementPlan
pub fn plan(spec: &JobSpec, map: &FabricMap) -> Result<PlacementPlan, PlanError> {
    if spec.payer.as_deref().unwrap_or("").is_empty() {
        return Err(PlanError::MissingPayer);
    }

    let graph = build_graph(map, &GraphOptions::default());
    let now = map.assembled_at_ms / 1000;

    let cand0 = feasible(map, spec, &graph, now);
    let cand = tag_candidates(cand0, Some(&graph));
    let seed = beacon_seed(&map.beacon, spec);
    let outcome = select(cand, spec, Some(&graph), seed);

    Ok(PlacementPlan {
        targets: outcome.targets,
        effective_k: outcome.effective_k,
        requested_k: spec_k(spec),
        beacon: map.beacon.clone(),
        weights: resolve_weights(spec),
        objective: spec.objective.unwrap_or_default().as_str().to_string(),
        relaxed: outcome.relaxed,
        shortfall: outcome.shortfall,
        rejected: outcome.rejected,
        assembled_at_ms: map.assembled_at_ms,
    })
}

// ----------------------------------------------------------------------------
// Tests — the JS placer.__selftest fixtures translated, pinned to JS-generated
// reference outputs (same stub graph, same synthetic scoreFn, same seeds).
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FabricNode, Objective, Weights};
    use crate::scorer::{BenchFit, FitSource, ScoreParts};

    const NOW: u64 = 10_000;

    /// The JS selftest stub graph: payer "me" measures us-a@5 / us-b@7 / eu-a@90; lone is reachable
    /// only via prediction (40); us-a/us-b co-region, eu-a its own region, lone region-less.
    struct StubGraph;
    impl LatencyView for StubGraph {
        fn measured_rtt(&self, a: &str, b: &str) -> Option<f64> {
            if a == b {
                return Some(0.0);
            }
            let key = |x: &str, y: &str| match (x, y) {
                ("me", "us-a") | ("us-a", "me") => Some(5.0),
                ("me", "us-b") | ("us-b", "me") => Some(7.0),
                ("me", "eu-a") | ("eu-a", "me") => Some(90.0),
                _ => None,
            };
            key(a, b)
        }
        fn predicted_rtt(&self, a: &str, b: &str) -> f64 {
            if a == b {
                return 0.0;
            }
            if let Some(m) = self.measured_rtt(a, b) {
                return m;
            }
            match (a, b) {
                ("me", "lone") | ("lone", "me") => 40.0,
                _ => f64::INFINITY,
            }
        }
        fn region_of(&self, node: &str) -> i64 {
            match node {
                "us-a" | "us-b" => 0,
                "eu-a" => 1,
                _ => -1,
            }
        }
    }

    fn node(id: &str, cpu: u32, mem: u64, jobs: u32, last_seen: u64, tags: &[&str]) -> FabricNode {
        FabricNode {
            node_id: id.into(),
            capacity: NodeCapacity {
                node_id: id.into(),
                cpu_cores: cpu,
                mem_mb: mem,
                running_jobs: jobs,
                last_seen_secs: last_seen,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                ask_base_units: None,
            },
            profile: None,
            history: None,
        }
    }

    fn history(jobs: u64, heartbeats: u64) -> NodeHistory {
        NodeHistory { jobs_hosted: jobs, heartbeats_hosted: heartbeats, ..Default::default() }
    }

    /// The JS selftest atlas: four good hosts + a stale, an undersized, an untagged, and the payer.
    /// us-a is a trusted veteran; us-b modest; eu-a fresh; lone unknown.
    fn fixture_map() -> FabricMap {
        let mut m = FabricMap {
            nodes: vec![
                node("us-a", 8, 16000, 0, NOW - 10, &["docker", "asn:64500"]),
                node("us-b", 8, 16000, 0, NOW - 10, &["docker", "asn:64500"]),
                node("eu-a", 8, 16000, 0, NOW - 10, &["docker", "asn:64600"]),
                node("lone", 8, 16000, 0, NOW - 10, &["docker"]),
                node("stale", 32, 64000, 0, NOW - 9999, &["docker"]), // liveness fail
                node("small", 1, 256, 0, NOW - 10, &["docker"]),      // headroom fail (needs 2c/512m)
                node("notag", 8, 16000, 0, NOW - 10, &["highmem"]),   // tag fail (needs docker)
                node("me", 8, 16000, 0, NOW - 10, &["docker"]),       // self → excluded
            ],
            assembled_at_ms: NOW * 1000,
            beacon: BeaconRef { height: 12345, hash: "deadbeefcafe".into() },
            ..Default::default()
        };
        m.nodes[0].history = Some(history(200, 5000));
        m.nodes[1].history = Some(history(2, 10));
        m.nodes[2].history = Some(history(0, 0));
        m
    }

    fn spec(k: u32) -> JobSpec {
        JobSpec {
            payer: Some("me".into()),
            k: Some(k),
            cpu_cores: 2,
            mem_mb: 512,
            require_tags: vec!["docker".into()],
            ..Default::default()
        }
    }

    /// The JS selftest's synthetic scoreFn: latency-dominated plus a small trust term.
    fn synthetic(c: &Candidate, _spec: &JobSpec, _g: Option<&dyn LatencyView>) -> StaticScore {
        let latency = clamp01(1.0 - c.rtt_ms / 250.0);
        let delivered = c.history.as_ref().map(|h| (h.jobs_hosted + h.heartbeats_hosted) as f64).unwrap_or(0.0);
        let trust = clamp01(delivered.ln_1p() / 50f64.ln_1p());
        let (bench_fit, price) = (0.5, 0.5);
        StaticScore {
            score: 0.7 * latency + 0.1 * bench_fit + 0.15 * trust + 0.05 * price,
            parts: ScoreParts { latency, bench_fit, trust, price, runtime_factor: 1.0 },
            bench_fit: BenchFit { score: bench_fit, source: FitSource::Atlas, confidence: 0.5 },
            benchmark_suspect: false,
        }
    }

    fn tagged_pool(spec_: &JobSpec) -> Vec<Candidate> {
        let cand = feasible(&fixture_map(), spec_, &StubGraph, NOW);
        tag_candidates(cand, Some(&StubGraph))
    }

    fn ids(targets: &[PlanTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.node_id.as_str()).collect()
    }

    // ---- 1) feasible() hard filter -----------------------------------------

    #[test]
    fn feasible_honors_every_hard_constraint() {
        let spec1 = spec(1);
        let feas = feasible(&fixture_map(), &spec1, &StubGraph, NOW);
        let feas_ids: HashSet<&str> = feas.iter().map(|c| c.node_id.as_str()).collect();
        for good in ["us-a", "us-b", "eu-a", "lone"] {
            assert!(feas_ids.contains(good), "the four good hosts are feasible ({good})");
        }
        assert!(!feas_ids.contains("stale"), "stale host excluded by liveness");
        assert!(!feas_ids.contains("small"), "undersized host excluded by headroom");
        assert!(!feas_ids.contains("notag"), "host missing required 'docker' tag excluded");
        assert!(!feas_ids.contains("me"), "payer self excluded by default");

        // Candidate shape: capacity carried, rtt_measured ground-truth where a direct sample exists.
        let us_a = feas.iter().find(|c| c.node_id == "us-a").unwrap();
        assert_eq!(us_a.capacity.cpu_cores, 8);
        assert_eq!(us_a.capacity.mem_mb, 16000);
        assert!(us_a.rtt_measured && us_a.rtt_ms == 5.0, "us-a uses the measured RTT (ground truth)");
        let lone = feas.iter().find(|c| c.node_id == "lone").unwrap();
        assert!(!lone.rtt_measured && lone.rtt_ms == 40.0, "lone uses the predicted RTT (no direct sample)");
        assert_eq!(us_a.history.as_ref().unwrap().jobs_hosted, 200, "history rides the map onto the candidate");

        // allow_self admits the payer; exclude removes a host.
        let spec_self = JobSpec { allow_self: true, exclude: vec!["eu-a".into()], ..spec(1) };
        let feas2 = feasible(&fixture_map(), &spec_self, &StubGraph, NOW);
        let ids2: HashSet<&str> = feas2.iter().map(|c| c.node_id.as_str()).collect();
        assert!(ids2.contains("me"), "allow_self admits the payer");
        assert!(!ids2.contains("eu-a"), "exclude removes a host");
    }

    #[test]
    fn feasible_capability_floor() {
        let mut map = fixture_map();
        // Give us-a a measured profile with a big GPU; eu-a stays profile-less.
        map.nodes[0].profile = Some(NodeProfile {
            node_id: "us-a".into(),
            gpus: vec![crate::api::GpuInfo { vram_mb: 24000.0, ..Default::default() }],
            ..Default::default()
        });
        let floor = JobSpec { min_vram_mb: Some(16000.0), ..spec(1) };
        let feas = feasible(&map, &floor, &StubGraph, NOW);
        let feas_ids: HashSet<&str> = feas.iter().map(|c| c.node_id.as_str()).collect();
        assert!(feas_ids.contains("us-a"), "profiled host clearing the floor stays");
        assert!(feas_ids.contains("eu-a"), "profile-less host admitted while require_profile=false");

        let strict = JobSpec { require_profile: true, ..floor };
        let feas2 = feasible(&map, &strict, &StubGraph, NOW);
        assert_eq!(feas2.len(), 1, "require_profile excludes every unmeasured host");
        assert_eq!(feas2[0].node_id, "us-a");

        // A measured profile BELOW the floor is excluded outright.
        map.nodes[0].profile.as_mut().unwrap().gpus[0].vram_mb = 8000.0;
        let feas3 = feasible(&map, &strict, &StubGraph, NOW);
        assert!(feas3.is_empty(), "below-floor profile excluded");
    }

    // ---- 2) beacon_seed determinism + JS parity -----------------------------

    #[test]
    fn beacon_seed_matches_js_bit_for_bit() {
        let beacon = BeaconRef { height: 12345, hash: "deadbeefcafe".into() };
        let req = |nonce: &str| JobSpec { payer: Some("me".into()), nonce: Some(nonce.into()), ..Default::default() };
        // JS reference values (node, src/placer.js beaconSeed).
        assert_eq!(beacon_seed(&beacon, &req("job-1")), 1922596420);
        assert_eq!(beacon_seed(&beacon, &req("job-2")), 904435639);
        assert_eq!(beacon_seed(&beacon, &req("job-1")), beacon_seed(&beacon, &req("job-1")), "deterministic");
        // Zero-everything still yields the JS seed (empty-string FNV offsets mix in).
        assert_eq!(beacon_seed(&BeaconRef::default(), &JobSpec::default()), 2147011751);
        // Heights past 2^32 truncate exactly like the JS `>>> 0` ToUint32.
        let big = BeaconRef { height: 4_294_967_297, hash: "ff".into() };
        assert_eq!(beacon_seed(&big, &JobSpec { payer: Some("p".into()), ..Default::default() }), 2156362244);
    }

    // ---- 3) redundancy_for --------------------------------------------------

    #[test]
    fn redundancy_for_policies() {
        let feas = tagged_pool(&spec(1));
        let with = |k: u32, red: Redundancy| JobSpec { redundancy: Some(red), ..spec(k) };

        assert_eq!(redundancy_for(&feas, &with(1, Redundancy::Policy(RedundancyPolicy::None))), 1, "none keeps effectiveK=k");
        // Best feasible trust = us-a (5200 delivered => ~saturated >= 0.6) so verify needs 2 replicas.
        assert_eq!(redundancy_for(&feas, &with(1, Redundancy::Policy(RedundancyPolicy::Verify))), 2, "verify with a high-trust pool => 2");
        // Pool of only low-trust hosts (eu-a fresh, lone unknown) => verify wants 3 replicas, but the
        // pool has only 2 hosts, so effectiveK is bounded by the pool size.
        let low: Vec<Candidate> = feas.iter().filter(|c| c.node_id != "us-a" && c.node_id != "us-b").cloned().collect();
        assert_eq!(low.len(), 2);
        assert_eq!(redundancy_for(&low, &with(1, Redundancy::Policy(RedundancyPolicy::Verify))), 2, "verify bounded by the 2-host pool");
        // k dominates when larger than the policy minimum.
        assert_eq!(redundancy_for(&feas, &with(4, Redundancy::Policy(RedundancyPolicy::Verify))), 4, "explicit k>policy wins");
        // Target confidence: an untrusted pool needs more replicas than a trusted one.
        let n_low = redundancy_for(&low, &with(1, Redundancy::Confidence(0.99)));
        let n_high = redundancy_for(&feas, &with(1, Redundancy::Confidence(0.99)));
        assert!(n_low >= n_high, "lower best-trust never needs fewer replicas ({n_low} vs {n_high})");
        assert!(n_high >= 1);
    }

    // ---- 4) select(): spread, low-RTT first, caps — pinned to the JS run ----

    fn spec4() -> JobSpec {
        JobSpec {
            redundancy: Some(Redundancy::Policy(RedundancyPolicy::Verify)),
            max_share: Some(0.34),
            objective: Some(Objective::Latency),
            nonce: Some("n4".into()),
            ..spec(3)
        }
    }

    fn seed4() -> u32 {
        beacon_seed(&BeaconRef { height: 12345, hash: "deadbeefcafe".into() }, &spec4())
    }

    #[test]
    fn select_matches_the_js_reference_run() {
        assert_eq!(seed4(), 1489896003, "seed4 matches the JS run");
        let res = select_with(tagged_pool(&spec4()), &spec4(), Some(&StubGraph), seed4(), &synthetic);
        // JS reference: us-a, us-b, lone — verify k=3 across distinct operators, lowest-RTT first.
        assert_eq!(ids(&res.targets), vec!["us-a", "us-b", "lone"]);
        let expect_scores = [0.91100000000000003, 0.85325337386710254, 0.66300000000000003];
        for (t, e) in res.targets.iter().zip(expect_scores) {
            assert!((t.score - e).abs() < 1e-12, "target static scores match the JS run ({} vs {e})", t.score);
        }
        assert!(res.relaxed.is_empty());
        assert_eq!(res.shortfall, 0);
        assert_eq!(res.rejected.len(), 1);
        assert_eq!(res.rejected[0].node_id, "eu-a");
        assert_eq!(res.rejected[0].reason, "low_score");
        // Replica indices + distinct operators (verify).
        let ops: HashSet<&str> = res.targets.iter().map(|t| t.groups.operator.as_str()).collect();
        assert_eq!(ops.len(), res.targets.len(), "all replicas on DISTINCT operators (verify)");
        for (i, t) in res.targets.iter().enumerate() {
            assert_eq!(t.replica, i as u32);
        }
    }

    #[test]
    fn select_strong_diversity_beats_raw_proximity() {
        // With a STRONG wD, the correlated us-b (shares us-a's asn+region+cluster) is pushed below
        // the independent lone@40 for the second slot, even though us-b is far closer (7ms vs 40ms).
        let spec_div = JobSpec {
            weights: Some(Weights { w_l: 0.5, w_b: 0.15, w_t: 0.2, w_p: 0.05, w_d: 1.5 }),
            ..spec4()
        };
        let res = select_with(tagged_pool(&spec_div), &spec_div, Some(&StubGraph), seed4(), &synthetic);
        // JS reference: us-a, lone, eu-a.
        assert_eq!(ids(&res.targets), vec!["us-a", "lone", "eu-a"]);
    }

    #[test]
    fn select_short_plan_reports_shortfall() {
        // k=6 under verify from 4 hosts => 4 placed, shortfall the rest.
        let spec5 = JobSpec {
            redundancy: Some(Redundancy::Policy(RedundancyPolicy::Verify)),
            nonce: Some("n5".into()),
            ..spec(6)
        };
        let seed5 = beacon_seed(&BeaconRef { height: 12345, hash: "deadbeefcafe".into() }, &spec5);
        let pool = tagged_pool(&spec5);
        let effective = redundancy_for(&pool, &spec5);
        let res = select_with(pool, &spec5, Some(&StubGraph), seed5, &synthetic);
        assert_eq!(res.targets.len(), 4, "verify across 4 distinct operators places 4");
        assert!(res.shortfall > 0, "an unsatisfiable replica count is reported as a shortfall");
        assert_eq!(res.shortfall, effective - res.targets.len() as u32, "shortfall = effectiveK - placed");
    }

    #[test]
    fn select_strict_cap_hard_rejects_correlated_hosts() {
        // k=4, maxShare tiny so cap=1 on every group: us-a and us-b cannot both be chosen.
        let spec6 = JobSpec {
            redundancy: Some(Redundancy::Policy(RedundancyPolicy::Verify)),
            max_share: Some(0.01),
            nonce: Some("n6".into()),
            ..spec(4)
        };
        let seed6 = beacon_seed(&BeaconRef { height: 12345, hash: "deadbeefcafe".into() }, &spec6);
        assert_eq!(seed6, 3534731105, "seed6 matches the JS run");
        let res = select_with(tagged_pool(&spec6), &spec6, Some(&StubGraph), seed6, &synthetic);
        // JS reference: us-a, lone, eu-a — never the correlated pair together; shortfall 1.
        assert_eq!(ids(&res.targets), vec!["us-a", "lone", "eu-a"]);
        assert_eq!(res.shortfall, 1);
        // Every rejected host carries a reason.
        for r in &res.rejected {
            assert!(!r.reason.is_empty(), "rejected host {} has a reason", r.node_id);
        }
    }

    // ---- 5) determinism + weighted selection --------------------------------

    #[test]
    fn select_is_deterministic_given_a_seed() {
        let a = select_with(tagged_pool(&spec4()), &spec4(), Some(&StubGraph), seed4(), &synthetic);
        let b = select_with(tagged_pool(&spec4()), &spec4(), Some(&StubGraph), seed4(), &synthetic);
        assert_eq!(ids(&a.targets), ids(&b.targets), "select is deterministic given a fixed seed");
    }

    #[test]
    fn select_weighted_matches_the_js_softmax_stream() {
        let spec_w = JobSpec { selection: Some(Selection::Weighted), temperature: Some(0.1), ..spec4() };
        let res = select_with(tagged_pool(&spec_w), &spec_w, Some(&StubGraph), seed4(), &synthetic);
        // JS reference (same seed, same softmax + PRNG stream): us-b, us-a, lone.
        assert_eq!(ids(&res.targets), vec!["us-b", "us-a", "lone"]);
        let ops: HashSet<&str> = res.targets.iter().map(|t| t.groups.operator.as_str()).collect();
        assert_eq!(ops.len(), 3, "weighted selection still honors distinct operators");
    }

    // ---- 6) cohort ----------------------------------------------------------

    #[test]
    fn cohort_colocate_prefers_the_same_region_neighbour() {
        // colocate, k=2, no verify (so operators may repeat) → after us-a, the same-region us-b is
        // preferred over the far eu-a despite the diversity penalty.
        let spec_c = JobSpec {
            redundancy: Some(Redundancy::Policy(RedundancyPolicy::None)),
            cohort: Some(Cohort::Colocate),
            objective: Some(Objective::Latency),
            max_share: Some(1.0),
            nonce: Some("nc".into()),
            ..spec(2)
        };
        let seed_c = beacon_seed(&BeaconRef { height: 12345, hash: "deadbeefcafe".into() }, &spec_c);
        assert_eq!(seed_c, 4288246340, "seedC matches the JS run");
        let res = select_with(tagged_pool(&spec_c), &spec_c, Some(&StubGraph), seed_c, &synthetic);
        // JS reference: us-a, us-b.
        assert_eq!(ids(&res.targets), vec!["us-a", "us-b"]);
    }

    // ---- 7) plan(): the pure end-to-end -------------------------------------

    #[test]
    fn plan_requires_payer() {
        let err = plan(&JobSpec { cpu_cores: 1, mem_mb: 256, ..Default::default() }, &FabricMap::default()).unwrap_err();
        assert_eq!(err, PlanError::MissingPayer);
        assert!(err.to_string().contains("payer"));
    }

    /// A real-map fixture (edges anchored at the payer, like the daemon's `/netgraph` fold) so plan()
    /// exercises the actual graph build.
    fn plan_map() -> FabricMap {
        let mut map = fixture_map();
        let edge = |b: &str, rtt_ms: f64| crate::api::MeshEdge {
            a: "me".into(),
            b: b.into(),
            rtt_ms,
            samples: 10,
            last_seen_secs: NOW,
        };
        map.nodes.retain(|n| !matches!(n.node_id.as_str(), "stale" | "small" | "notag"));
        map.edges = vec![edge("us-a", 5.0), edge("us-b", 7.0), edge("eu-a", 90.0), edge("lone", 40.0)];
        map
    }

    #[test]
    fn plan_is_deterministic_and_auditable() {
        let map = plan_map();
        let spec_p = JobSpec {
            redundancy: Some(Redundancy::Policy(RedundancyPolicy::Verify)),
            objective: Some(Objective::Latency),
            nonce: Some("job-42".into()),
            ..spec(2)
        };
        let a = plan(&spec_p, &map).unwrap();
        let b = plan(&spec_p, &map).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "(spec, map) -> identical plan, bit for bit"
        );
        assert!(!a.targets.is_empty(), "the fixture pool satisfies the request");
        assert_eq!(a.requested_k, 2);
        assert!(a.effective_k >= a.requested_k);
        assert_eq!(a.beacon, map.beacon, "the plan echoes the beacon it was seeded from");
        assert_eq!(a.objective, "latency");
        assert_eq!(a.assembled_at_ms, map.assembled_at_ms);
        // Verify forces distinct operators.
        let ops: HashSet<&str> = a.targets.iter().map(|t| t.groups.operator.as_str()).collect();
        assert_eq!(ops.len(), a.targets.len());
        // A different nonce may reorder ties but stays a valid, fully-placed plan.
        let spec_q = JobSpec { nonce: Some("job-43".into()), ..spec_p.clone() };
        let c = plan(&spec_q, &map).unwrap();
        assert_eq!(c.targets.len(), a.targets.len());
    }

    #[test]
    fn plan_infeasible_spec_yields_empty_short_plan() {
        let map = plan_map();
        // Nothing has 999 cores: valid request, empty plan, full shortfall — not an error.
        let impossible = JobSpec { cpu_cores: 999, ..spec(2) };
        let p = plan(&impossible, &map).unwrap();
        assert!(p.targets.is_empty(), "no feasible host => empty plan");
        assert_eq!(p.shortfall, p.effective_k, "everything requested is missing");
        assert!(p.shortfall >= 2);
        // An empty map behaves the same.
        let empty = plan(&spec(1), &FabricMap::default()).map_err(|_| ()).ok();
        assert!(empty.is_none() || empty.as_ref().unwrap().targets.is_empty());
    }
}
