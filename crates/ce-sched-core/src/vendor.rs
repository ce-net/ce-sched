//! Vendor / diversity / risk model — pure functions over candidates + the chosen set. No I/O.
//!
//! Rust port of the JS `src/vendor.js`. Stops a job (and a payer's recent jobs) from piling onto one
//! operator or one correlated cluster. Provides the grouping keys, the concentration caps, and the
//! contextual diversity penalty the placer applies during selection (§4 of `placement-design.md`).
//! Reads only public facts (the graph's latency regions + a node's profile/atlas tags); never I/O.
//!
//! CONTRACT (mirrors the JS module):
//!
//! - [`group_keys`] — `{operator, asn, region, cluster}` from public facts.
//! - [`tag_candidates`] — attach `.groups` to every candidate, resolve union-find clusters.
//! - [`per_group_cap`] — `ceil(effectiveK * maxShare)`, min 1.
//! - [`diversity_penalty`] — `[0,~2]`: within-job groupLoad + across-job recentLoad.
//! - [`violates_cap`] — the offending [`GroupKey`] if adding a candidate breaks a hard cap.
//! - [`cluster_of`] — union-find label per node over operator/asn/region overlap.
//!
//! `operator` collapses two nodes under one on-chain owner into one group (verifying across one
//! owner's two boxes proves nothing); `asn`/`region` are weaker correlation buckets, only counted
//! when they are real signals (not a sentinel) so region-less / asn-less hosts are not spuriously
//! capped.

use crate::api::Groups;
use crate::graph::LatencyView;
use crate::placer::Candidate;
use std::collections::HashMap;

/// Sentinel grouping value when a key cannot be derived from public facts.
pub const UNKNOWN: &str = "unknown";

// ----------------------------------------------------------------------------
// Grouping keys (§4).
// ----------------------------------------------------------------------------

/// Best-available ASN (network-provider) proxy from public facts: an `asn:<x>` capability tag on the
/// atlas entry (operators may self-tag; correlated-outage proxy), else [`UNKNOWN`]. The JS engine
/// also honored a future `profile.runtime.asn` hint; the frozen Rust `RuntimeInfo` does not carry
/// one yet, so the tag is the only real signal today. Treated purely as a correlation bucket, never
/// trusted for authorization.
fn asn_of(candidate: &Candidate) -> String {
    for tag in &candidate.capacity.tags {
        let lower = tag.to_lowercase();
        if let Some(rest) = lower.strip_prefix("asn:") {
            let a = tag[tag.len() - rest.len()..].trim();
            if !a.is_empty() {
                return a.to_string();
            }
        }
    }
    UNKNOWN.to_string()
}

/// Best-available operator (controller / owner) proxy. Today the node id IS the operator key (one
/// key per node); when `/history` carries an on-chain `owner`/`operator` record it wins, so two
/// nodes under one owner collapse to one operator group (verification across them proves nothing).
fn operator_of(candidate: &Candidate) -> String {
    if let Some(h) = &candidate.history {
        for owner in [&h.owner, &h.operator] {
            if let Some(o) = owner.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                return o.to_string();
            }
        }
    }
    if candidate.node_id.is_empty() { UNKNOWN.to_string() } else { candidate.node_id.clone() }
}

/// Grouping keys for one candidate. Region uses the graph's O(1) `region_of` (a measured-latency
/// cluster = a same-DC/LAN correlation proxy); `-1` (unknown region) maps to a per-node region key
/// so region-less nodes never accidentally share a region with each other.
pub fn group_keys(candidate: &Candidate, graph: Option<&dyn LatencyView>) -> Groups {
    let operator = operator_of(candidate);
    let asn = asn_of(candidate);
    let ridx = graph.map(|g| g.region_of(&candidate.node_id)).unwrap_or(-1);
    // -1 = no measured region; give it a node-unique key so two unplaced nodes are NOT co-region.
    let region = if ridx >= 0 { format!("r{ridx}") } else { format!("r?:{}", candidate.node_id) };
    // cluster is filled by cluster_of() union-find when the whole candidate set is known; for a lone
    // candidate it degenerates to the {operator|asn|region} composite (its own broadest bucket).
    let cluster = format!("{operator}|{asn}|{region}");
    Groups { operator, asn, region, cluster }
}

/// Attach `.groups` to every candidate and resolve the union-find `cluster` label across the whole
/// pool (so transitively-correlated candidates share one cluster). Consumes and returns the pool —
/// the placer wants a stable enriched list.
pub fn tag_candidates(mut candidates: Vec<Candidate>, graph: Option<&dyn LatencyView>) -> Vec<Candidate> {
    // First pass: per-candidate keys (operator/asn/region + provisional cluster).
    for c in &mut candidates {
        c.groups = Some(group_keys(c, graph));
    }
    // Second pass: union-find collapses any two candidates sharing operator OR asn OR region into
    // one cluster label, so the broadest correlation bucket is transitive.
    let cluster = cluster_of(&candidates);
    for c in &mut candidates {
        if let (Some(label), Some(g)) = (cluster.get(&c.node_id), c.groups.as_mut()) {
            g.cluster = label.clone();
        }
    }
    candidates
}

// ----------------------------------------------------------------------------
// Concentration caps (§4).
// ----------------------------------------------------------------------------

/// Max candidates from any single group allowed in one job: `ceil(effectiveK * maxShare)`, floored
/// at 1 (a group may always hold at least one host, else nothing is placeable). Default maxShare
/// 0.34 ⇒ no group exceeds ~1/3, so a k≥3 job spans several independent operators. A non-finite
/// share defaults to 0.34; a non-positive share floors the cap to 1; a share `> 1` clamps to 1.
pub fn per_group_cap(effective_k: u32, max_share: f64) -> u32 {
    let k = if effective_k > 0 { effective_k } else { 1 } as f64;
    let mut share = if max_share.is_finite() { max_share } else { 0.34 };
    if share <= 0.0 {
        return 1;
    }
    if share > 1.0 {
        share = 1.0;
    }
    let cap = (k * share).ceil() as u32;
    cap.max(1)
}

/// How many of `chosen` already share each group with `candidate`. Used both by the soft penalty
/// (groupLoad) and the hard cap (violates_cap).
struct GroupCounts {
    operator: u32,
    asn: u32,
    region: u32,
    cluster: u32,
    /// How many of `chosen` share AT LEAST ONE group.
    any: u32,
}

fn group_counts(candidate: &Candidate, chosen: &[&Candidate]) -> GroupCounts {
    let mut counts = GroupCounts { operator: 0, asn: 0, region: 0, cluster: 0, any: 0 };
    let Some(cg) = &candidate.groups else { return counts };
    for ch in chosen {
        let Some(g) = &ch.groups else { continue };
        let mut shared = false;
        if g.operator == cg.operator {
            counts.operator += 1;
            shared = true;
        }
        // asn/region/cluster only count as correlation when the value is a real signal (not
        // "unknown" and not a per-node region sentinel) — otherwise every region-less / asn-less
        // node would look mutually correlated and the caps would over-fire.
        if cg.asn != UNKNOWN && g.asn == cg.asn {
            counts.asn += 1;
            shared = true;
        }
        if !cg.region.starts_with("r?:") && g.region == cg.region {
            counts.region += 1;
            shared = true;
        }
        if g.cluster == cg.cluster {
            counts.cluster += 1;
            shared = true;
        }
        if shared {
            counts.any += 1;
        }
    }
    counts
}

// ----------------------------------------------------------------------------
// Soft penalty (§2 wD term, §4).
// ----------------------------------------------------------------------------

/// Contextual diversity penalty in roughly `[0, 2]`:
///
/// ```text
/// diversity_penalty = groupLoad + recentLoad
///   groupLoad  = (# of chosen sharing ANY group with c) / max(1, |chosen|)   // within-job spread
///   recentLoad = recent_placements[c.operator] / Σ recent_placements         // across-job spread
/// ```
///
/// Subtracted (times `wD`) from the live score during selection, so the second-best-but-DIFFERENT
/// host beats the second-best-but-SAME host. Returns 0 for the first pick (empty `chosen`) when the
/// payer has no recent history, so an unconstrained job is unaffected.
pub fn diversity_penalty(candidate: &Candidate, chosen: &[&Candidate], spec: &crate::api::JobSpec) -> f64 {
    let counts = group_counts(candidate, chosen);
    let group_load = if !chosen.is_empty() { counts.any as f64 / chosen.len() as f64 } else { 0.0 };

    let mut recent_load = 0.0;
    if let Some(recent) = &spec.recent_placements {
        let mut total = 0.0;
        for v in recent.values() {
            if v.is_finite() && *v > 0.0 {
                total += v;
            }
        }
        if total > 0.0 {
            let op = candidate.groups.as_ref().map(|g| g.operator.clone()).unwrap_or_else(|| operator_of(candidate));
            if let Some(mine) = recent.get(&op).filter(|m| m.is_finite() && **m > 0.0) {
                recent_load = mine / total;
            }
        }
    }
    group_load + recent_load
}

// ----------------------------------------------------------------------------
// Hard caps (§5b).
// ----------------------------------------------------------------------------

/// The four correlation group keys a hard cap can fire on (the JS string returns, typed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKey {
    Operator,
    Asn,
    Region,
    Cluster,
}

impl GroupKey {
    /// The wire/reason string for this key.
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupKey::Operator => "operator",
            GroupKey::Asn => "asn",
            GroupKey::Region => "region",
            GroupKey::Cluster => "cluster",
        }
    }
}

/// The hard-cap parameters `violates_cap` enforces.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Max chosen hosts per group (`u32::MAX` = uncapped).
    pub per_group: u32,
    /// Distinct-operator gate: enforced for `redundancy = "verify"`.
    pub require_distinct_operator: bool,
}

/// Would adding `candidate` to `chosen` break a HARD constraint? Returns the offending [`GroupKey`]
/// so the placer can record a precise rejection reason / decide which cap to relax, or `None` if the
/// candidate is admissible.
///
/// Two hard gates:
///  1. distinct-operator (redundancy="verify"): a replica may NOT share an operator with any chosen
///     replica — verification across one operator's two boxes proves nothing. Checked first because
///     it is non-negotiable and independent of the count cap.
///  2. per-group count cap: adding c must not push the count in any of its groups past
///     `caps.per_group`. Real-signal groups only (operator always; asn/region only when not a
///     sentinel) so region-less / asn-less hosts are not spuriously capped together.
pub fn violates_cap(candidate: &Candidate, chosen: &[&Candidate], caps: &Caps) -> Option<GroupKey> {
    let counts = group_counts(candidate, chosen);
    let cg = candidate.groups.as_ref();

    // 1. distinct-operator (verify) — strongest, count-independent.
    if caps.require_distinct_operator && counts.operator > 0 {
        return Some(GroupKey::Operator);
    }

    // 2. per-group count caps. `counts.x` is how many chosen ALREADY share group x; adding c makes
    //    it counts.x + 1, which must stay <= per_group.
    let per_group = caps.per_group;
    if counts.operator.saturating_add(1) > per_group {
        return Some(GroupKey::Operator);
    }
    if let Some(cg) = cg {
        if cg.asn != UNKNOWN && counts.asn.saturating_add(1) > per_group {
            return Some(GroupKey::Asn);
        }
        if !cg.region.starts_with("r?:") && counts.region.saturating_add(1) > per_group {
            return Some(GroupKey::Region);
        }
    }
    if counts.cluster.saturating_add(1) > per_group {
        return Some(GroupKey::Cluster);
    }
    None
}

// ----------------------------------------------------------------------------
// Union-find clustering (§4).
// ----------------------------------------------------------------------------

/// Union-find over the candidate pool: any two candidates that share an operator, a (real) asn, or a
/// (real) region are joined into one cluster. The returned map gives each node a stable cluster
/// label (the representative node id of its set), so transitively-correlated hosts collapse to one
/// bucket — the broadest correlation group used by `groups.cluster`.
///
/// Candidates must already carry `.groups` (call after [`group_keys`] / within [`tag_candidates`]);
/// a candidate lacking groups is keyed on its own facts (its own singleton unless it shares them).
pub fn cluster_of(candidates: &[Candidate]) -> HashMap<String, String> {
    let mut parent: HashMap<String, String> = HashMap::new();

    fn find(parent: &mut HashMap<String, String>, x: &str) -> String {
        let mut root = x.to_string();
        while parent[&root] != root {
            root = parent[&root].clone();
        }
        // Path-compress.
        let mut cur = x.to_string();
        while parent[&cur] != root {
            let next = parent[&cur].clone();
            parent.insert(cur, root.clone());
            cur = next;
        }
        root
    }

    for c in candidates {
        parent.insert(c.node_id.clone(), c.node_id.clone());
    }

    // Index by each real grouping value, unioning every node that shares it.
    let mut first_by_key: HashMap<String, String> = HashMap::new();
    {
        let mut link = |parent: &mut HashMap<String, String>, ns: &str, val: &str, node: &str| {
            if val.is_empty() {
                return;
            }
            let key = format!("{ns}={val}");
            match first_by_key.get(&key) {
                None => {
                    first_by_key.insert(key, node.to_string());
                }
                Some(prev) => {
                    let ra = find(parent, &prev.clone());
                    let rb = find(parent, node);
                    if ra != rb {
                        parent.insert(ra, rb);
                    }
                }
            }
        };
        for c in candidates {
            let owned;
            let g = match &c.groups {
                Some(g) => g,
                None => {
                    owned = group_keys(c, None);
                    &owned
                }
            };
            link(&mut parent, "op", &g.operator, &c.node_id);
            if g.asn != UNKNOWN {
                link(&mut parent, "asn", &g.asn, &c.node_id);
            }
            if !g.region.starts_with("r?:") {
                link(&mut parent, "region", &g.region, &c.node_id);
            }
        }
    }

    let mut out = HashMap::new();
    for c in candidates {
        let root = find(&mut parent, &c.node_id);
        out.insert(c.node_id.clone(), root);
    }
    out
}

// ----------------------------------------------------------------------------
// Tests — the JS vendor.__selftest fixtures translated (stub regions, no embedding).
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{JobSpec, NodeCapacity, NodeHistory};
    use std::collections::BTreeMap;

    /// Stub latency view: us-a/us-b => region 0 (same DC); eu-a => region 1; lone => none (-1).
    struct StubRegions;
    impl LatencyView for StubRegions {
        fn measured_rtt(&self, _: &str, _: &str) -> Option<f64> {
            None
        }
        fn predicted_rtt(&self, _: &str, _: &str) -> f64 {
            f64::INFINITY
        }
        fn region_of(&self, node: &str) -> i64 {
            match node {
                "us-a" | "us-b" => 0,
                "eu-a" => 1,
                _ => -1,
            }
        }
    }

    fn mk(id: &str, rtt_ms: f64, tags: &[&str]) -> Candidate {
        Candidate {
            node_id: id.into(),
            capacity: NodeCapacity {
                node_id: id.into(),
                cpu_cores: 8,
                mem_mb: 16000,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            },
            profile: None,
            history: None,
            rtt_ms,
            rtt_measured: true,
            ask_base_units: None,
            groups: None,
        }
    }

    /// The four-host fixture pool: two correlated US hosts, an independent EU host, a lone host.
    fn pool() -> Vec<Candidate> {
        tag_candidates(
            vec![
                mk("us-a", 5.0, &["docker", "asn:64500"]),
                mk("us-b", 7.0, &["docker", "asn:64500"]),
                mk("eu-a", 90.0, &["docker", "asn:64600"]),
                mk("lone", 40.0, &["docker"]),
            ],
            Some(&StubRegions),
        )
    }

    fn by_id<'a>(pool: &'a [Candidate], id: &str) -> &'a Candidate {
        pool.iter().find(|c| c.node_id == id).unwrap()
    }

    #[test]
    fn group_keys_and_clusters() {
        let pool = pool();
        let g = |id: &str| by_id(&pool, id).groups.as_ref().unwrap();
        assert_eq!(g("us-a").operator, "us-a", "operator key = node id");
        assert_eq!(g("us-a").asn, "64500", "asn parsed from asn: tag");
        assert_eq!(g("eu-a").asn, "64600", "eu-a distinct asn");
        assert_eq!(g("lone").asn, UNKNOWN, "no asn tag => unknown");
        assert_eq!(g("us-a").region, "r0");
        assert_eq!(g("us-b").region, "r0", "us-a/us-b co-region");
        assert_eq!(g("eu-a").region, "r1");
        assert!(g("lone").region.starts_with("r?:"), "region-less node gets unique region sentinel");

        // us-a & us-b share asn AND region => same union-find cluster; eu-a & lone are separate.
        assert_eq!(g("us-a").cluster, g("us-b").cluster, "us-a/us-b cluster together");
        assert_ne!(g("us-a").cluster, g("eu-a").cluster, "eu-a in a different cluster");
        assert_ne!(g("lone").cluster, g("us-a").cluster, "lone is its own cluster");
    }

    #[test]
    fn per_group_cap_matches_js() {
        assert_eq!(per_group_cap(3, 0.34), 2); // ceil(3*0.34)=2
        assert_eq!(per_group_cap(1, 0.34), 1); // k=1 cap is 1
        assert_eq!(per_group_cap(9, 0.34), 4); // ceil(9*0.34)=4
        assert_eq!(per_group_cap(3, 0.0), 1); // degenerate share floored to 1
        assert_eq!(per_group_cap(3, 2.0), 3); // share>1 clamps to 1 => cap = k
        assert_eq!(per_group_cap(0, 0.34), 1); // k<=0 treated as 1
        assert_eq!(per_group_cap(3, f64::NAN), 2); // non-finite share defaults to 0.34
    }

    #[test]
    fn diversity_penalty_prefers_independent_hosts() {
        let pool = pool();
        let spec = JobSpec::default();
        // After choosing us-a: us-b shares asn+region+cluster => penalized; eu-a/lone share nothing.
        let chosen = [by_id(&pool, "us-a")];
        let pen_us_b = diversity_penalty(by_id(&pool, "us-b"), &chosen, &spec);
        let pen_eu_a = diversity_penalty(by_id(&pool, "eu-a"), &chosen, &spec);
        let pen_lone = diversity_penalty(by_id(&pool, "lone"), &chosen, &spec);
        assert!(pen_us_b > 0.0, "us-b penalized for sharing asn/region/cluster with chosen us-a");
        assert_eq!(pen_eu_a, 0.0, "eu-a unpenalized (fully independent of us-a)");
        assert_eq!(pen_lone, 0.0, "lone unpenalized (fully independent of us-a)");
        assert!(pen_us_b > pen_eu_a, "correlated host is penalized more than an independent one");

        // recentLoad: payer has placed heavily on eu-a recently => eu-a gains a penalty even when
        // independent within THIS job.
        let recent_spec = JobSpec {
            recent_placements: Some(BTreeMap::from([("eu-a".to_string(), 8.0), ("us-a".to_string(), 2.0)])),
            ..Default::default()
        };
        let pen_recent = diversity_penalty(by_id(&pool, "eu-a"), &[], &recent_spec);
        assert!((pen_recent - 0.8).abs() < 1e-9, "recentLoad = 8/10 for eu-a across-user concentration");
        assert_eq!(diversity_penalty(by_id(&pool, "lone"), &[], &recent_spec), 0.0, "lone has no recent concentration");
    }

    #[test]
    fn violates_cap_per_group_and_distinct_operator() {
        let pool = pool();
        // cap per_group=1 (strict spread): once us-a chosen, us-b violates on asn/region/cluster
        // (shares them) though operator differs.
        let strict = Caps { per_group: 1, require_distinct_operator: false };
        assert!(violates_cap(by_id(&pool, "us-b"), &[by_id(&pool, "us-a")], &strict).is_some(), "us-b breaks per_group=1 vs us-a");
        assert_eq!(violates_cap(by_id(&pool, "eu-a"), &[by_id(&pool, "us-a")], &strict), None, "eu-a fits (independent)");

        // distinct-operator (verify): the SAME operator is rejected even if per_group is loose.
        // A second node under the same owner via history.owner exercises the operator-collapse path.
        let mut dup = mk("us-d", 6.0, &["docker"]);
        dup.history = Some(NodeHistory { owner: Some("us-a".into()), ..Default::default() });
        let dup_tagged = tag_candidates(vec![mk("us-a", 5.0, &["docker", "asn:64500"]), dup], Some(&StubRegions));
        assert_eq!(dup_tagged[1].groups.as_ref().unwrap().operator, "us-a", "history.owner collapses us-d into operator us-a");
        let verify = Caps { per_group: 99, require_distinct_operator: true };
        assert_eq!(
            violates_cap(&dup_tagged[1], &[&dup_tagged[0]], &verify),
            Some(GroupKey::Operator),
            "verify rejects same-operator replica"
        );
        assert_eq!(violates_cap(by_id(&pool, "eu-a"), &[by_id(&pool, "us-a")], &verify), None, "verify admits distinct operator");
    }

    /// End-to-end greedy spread mirroring placer §5 wiring: distinct operators, cap respected,
    /// the independent host beats the correlated near host on the soft penalty.
    #[test]
    fn greedy_spread_soft_penalty() {
        let pool = pool();
        let effective_k = 3usize;
        let cap = per_group_cap(effective_k as u32, 0.34); // = 2
        let w_d = 0.15;
        let latency = |c: &Candidate| 1.0 - c.rtt_ms / 250.0;
        let spec = JobSpec::default();

        let mut picked: Vec<&Candidate> = Vec::new();
        while picked.len() < effective_k {
            let mut best: Option<&Candidate> = None;
            let mut best_score = f64::NEG_INFINITY;
            for c in &pool {
                if picked.iter().any(|p| p.node_id == c.node_id) {
                    continue;
                }
                if violates_cap(c, &picked, &Caps { per_group: cap, require_distinct_operator: true }).is_some() {
                    continue;
                }
                let live = latency(c) - w_d * diversity_penalty(c, &picked, &spec);
                if live > best_score {
                    best_score = live;
                    best = Some(c);
                }
            }
            let Some(best) = best else { break };
            picked.push(best);
        }

        let ids: Vec<&str> = picked.iter().map(|c| c.node_id.as_str()).collect();
        assert_eq!(ids[0], "us-a", "lowest-RTT host picked first");
        // The independent low-RTT host (lone@40) outranks the correlated us-b@7 on the SECOND pick:
        // us-b's full groupLoad penalty (shares asn+region+cluster with us-a) sinks it below lone.
        assert_eq!(ids[1], "lone", "independent lone beats correlated us-b on the soft penalty");
        // All picks distinct operators; no operator exceeds the cap.
        let ops: Vec<&str> = picked.iter().map(|c| c.groups.as_ref().unwrap().operator.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = ops.iter().collect();
        assert_eq!(unique.len(), ops.len(), "all picks are on DISTINCT operators (verify spread)");
    }

    /// Strict-cap pass: the HARD per-group cap (=1) excludes correlated hosts outright.
    #[test]
    fn greedy_strict_cap_hard_excludes_correlated() {
        let pool = pool();
        let spec = JobSpec::default();
        let latency = |c: &Candidate| 1.0 - c.rtt_ms / 250.0;
        let mut picked: Vec<&Candidate> = Vec::new();
        while picked.len() < 3 {
            let mut best: Option<&Candidate> = None;
            let mut best_score = f64::NEG_INFINITY;
            for c in &pool {
                if picked.iter().any(|p| p.node_id == c.node_id) {
                    continue;
                }
                if violates_cap(c, &picked, &Caps { per_group: 1, require_distinct_operator: true }).is_some() {
                    continue;
                }
                let live = latency(c) - 0.15 * diversity_penalty(c, &picked, &spec);
                if live > best_score {
                    best_score = live;
                    best = Some(c);
                }
            }
            let Some(best) = best else { break };
            picked.push(best);
        }
        let ids: Vec<&str> = picked.iter().map(|c| c.node_id.as_str()).collect();
        assert!(!ids.contains(&"us-b"), "per_group=1 hard-excludes us-b (shares asn/region/cluster with us-a)");
        assert!(
            ids.contains(&"us-a") && ids.contains(&"eu-a") && ids.contains(&"lone"),
            "strict spread selects the three mutually-independent operators"
        );
    }
}
