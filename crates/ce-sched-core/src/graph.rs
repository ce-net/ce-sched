//! Latency queries over the assembled [`FabricMap`](crate::api::FabricMap) — pure, no I/O.
//!
//! Rust port of the JS `src/graph.js` (a standalone reimplementation of the `@ce-net/graph`
//! concepts): a Vivaldi/MDS spring embedding, union-find latency regions, and Dijkstra shortest paths,
//! built from the map's measured [`MeshEdge`](crate::api::MeshEdge)s + per-node
//! [`NodeCapacity`](crate::api::NodeCapacity).
//!
//! CONTRACT (mirrors the JS `Graph` class so the scorer/vendor/placer ported unchanged):
//!
//! ```text
//! build_graph(map, options) -> Graph
//! impl Graph {
//!   nodes()            -> &[NodeId]
//!   has(node)          -> bool
//!   measured_rtt(a,b)  -> Option<f64>   // ground-truth direct sample, order-independent
//!   predicted_rtt(a,b) -> f64           // embedding distance; shortest-path fallback; INF if unreachable
//!   k_nearest(node,k)  -> Vec<NodeId>   // ascending predicted RTT, excludes node + unreachable
//!   regions()          -> &[Vec<NodeId>]
//!   region_of(node)    -> i64           // stable region index; -1 if unknown (O(1) vendor grouping key)
//!   shortest_path(a,b) -> Vec<NodeId>
//!   capacity_of(node)  -> Option<&NodeCapacity>
//! }
//! ```
//!
//! Determinism: the embedding uses the seedable [`Mulberry32`] PRNG (bit-for-bit the JS mulberry32 —
//! `Math.imul`/`>>>` become `u32` wrapping ops, division by 2^32 stays f64), so `(map, seed) ->
//! identical coordinates`. Node/edge insertion order is preserved (JS `Map`/`Set` iterate insertion
//! order) so the float accumulation order — and therefore every coordinate — matches the JS engine.
//!
//! The scorer/placer consume the graph through the narrow [`LatencyView`] trait (the exact duck-typed
//! surface the JS modules used), so tests can stub latency without building an embedding.

use crate::api::{FabricMap, MeshEdge, NodeCapacity, NodeId};
use std::collections::{HashMap, HashSet};

// ----------------------------------------------------------------------------
// PRNG (shared with the placer's beacon-seeded tie-break).
// ----------------------------------------------------------------------------

/// mulberry32 — tiny deterministic PRNG, bit-for-bit the JS generator: state advances with 32-bit
/// wrapping adds/multiplies (`Math.imul` semantics) and yields `u32 / 2^32` as an `f64` in `[0,1)`.
#[derive(Debug, Clone)]
pub struct Mulberry32(u32);

impl Mulberry32 {
    /// Seed the stream (the JS `seed >>> 0` coercion is the `u32` type itself).
    pub fn new(seed: u32) -> Self {
        Mulberry32(seed)
    }

    /// Next float in `[0,1)`.
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let s = self.0;
        let mut t = (s ^ (s >> 15)).wrapping_mul(1 | s);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        ((t ^ (t >> 14)) as f64) / 4_294_967_296.0
    }
}

// ----------------------------------------------------------------------------
// The narrow latency surface the policy modules consume (JS duck-typing made explicit).
// ----------------------------------------------------------------------------

/// The three graph queries scorer/vendor/placer actually use. [`Graph`] implements it; unit tests
/// stub it with fixture maps (exactly how the JS self-tests stubbed the graph object).
pub trait LatencyView {
    /// Directly measured RTT (ms), order-independent; `None` if no direct sample exists.
    fn measured_rtt(&self, a: &str, b: &str) -> Option<f64>;
    /// Predicted RTT (ms); `f64::INFINITY` if nothing relates the pair.
    fn predicted_rtt(&self, a: &str, b: &str) -> f64;
    /// Stable latency-region index; `-1` if the node is unknown.
    fn region_of(&self, node: &str) -> i64;
}

// ----------------------------------------------------------------------------
// Embedding (Vivaldi / MDS-by-spring-relaxation). Port of graph.js `embed`.
// ----------------------------------------------------------------------------

/// Embedding tuning knobs (JS `EmbeddingOptions`).
#[derive(Debug, Clone)]
pub struct EmbeddingOptions {
    /// Embedding dimensionality.
    pub dimensions: usize,
    /// Spring-relaxation iterations.
    pub iterations: usize,
    /// Initial force fraction per update.
    pub initial_step: f64,
    /// Step-size floor.
    pub min_step: f64,
    /// Deterministic layout seed.
    pub seed: u32,
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        EmbeddingOptions { dimensions: 2, iterations: 300, initial_step: 0.25, min_step: 0.01, seed: 1 }
    }
}

/// Options for [`build_graph`] / [`Graph::build`].
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// Measured edges at or under this RTT join two nodes into one latency region.
    pub region_threshold_ms: f64,
    pub embedding: EmbeddingOptions,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions { region_threshold_ms: 30.0, embedding: EmbeddingOptions::default() }
    }
}

/// Euclidean distance between two equal-length vectors.
fn vec_distance(a: &[f64], b: &[f64]) -> f64 {
    let mut sum = 0.0;
    for i in 0..a.len() {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum.sqrt()
}

/// Compute Vivaldi/MDS coordinates for every node in `nodes` (insertion order preserved — it drives
/// the PRNG consumption order, so identical inputs + seed reproduce the JS layout bit-for-bit).
/// Isolated nodes still receive a seeded random coordinate so the result covers every node passed.
pub fn embed(nodes: &[NodeId], edges: &[MeshEdge], options: &EmbeddingOptions) -> HashMap<NodeId, Vec<f64>> {
    let dim = options.dimensions.max(1);
    let iterations = options.iterations.max(1);
    let initial_step = options.initial_step;
    let min_step = options.min_step;
    let mut rand = Mulberry32::new(options.seed);

    // Insertion-ordered node index (JS Map).
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut ordered: Vec<&str> = Vec::new();
    for n in nodes {
        if !index.contains_key(n.as_str()) {
            index.insert(n.as_str(), ordered.len());
            ordered.push(n.as_str());
        }
    }
    let n = ordered.len();

    let mean_rtt = if !edges.is_empty() {
        edges.iter().fold(0.0, |acc, e| acc + e.rtt_ms) / edges.len() as f64
    } else {
        50.0
    };

    let mut positions: Vec<Vec<f64>> = Vec::with_capacity(n);
    for _ in 0..n {
        let mut v = Vec::with_capacity(dim);
        for _ in 0..dim {
            v.push((rand.next() - 0.5) * mean_rtt);
        }
        positions.push(v);
    }

    // Springs: (i, j, target rtt, confidence from sample count).
    let mut e2: Vec<(usize, usize, f64, f64)> = Vec::new();
    for e in edges {
        let (Some(&i), Some(&j)) = (index.get(e.a.as_str()), index.get(e.b.as_str())) else { continue };
        if i == j {
            continue;
        }
        let conf = if e.samples > 0 { e.samples as f64 / (e.samples as f64 + 4.0) } else { 0.25 };
        e2.push((i, j, e.rtt_ms.max(0.001), conf));
    }

    for it in 0..iterations {
        let step = initial_step
            - (initial_step - min_step) * (it as f64 / (iterations.saturating_sub(1)).max(1) as f64);
        for &(i, j, w, conf) in &e2 {
            let d = vec_distance(&positions[i], &positions[j]);
            let err = d - w;
            if d < 1e-9 {
                // Coincident points: nudge apart along a deterministic axis.
                let axis = (i + j) % dim;
                positions[i][axis] -= 0.5;
                positions[j][axis] += 0.5;
                continue;
            }
            let force = step * conf * err;
            for k in 0..dim {
                let unit = (positions[i][k] - positions[j][k]) / d;
                positions[i][k] -= force * unit;
                positions[j][k] += force * unit;
            }
        }
    }

    let mut out = HashMap::new();
    for (id, idx) in &index {
        out.insert((*id).to_string(), positions[*idx].clone());
    }
    out
}

// ----------------------------------------------------------------------------
// build_graph + Graph.
// ----------------------------------------------------------------------------

/// Order-independent key for an undirected pair (JS `pairKey`).
fn pair_key(a: &str, b: &str) -> String {
    if a < b { format!("{a} {b}") } else { format!("{b} {a}") }
}

/// Assemble a queryable [`Graph`] from the map's edges (treated as directed observations — a
/// bidirectional pair fuses with a sample-weighted mean, exactly the JS `Graph.build`) and per-node
/// capacity rows.
pub fn build_graph(map: &FabricMap, options: &GraphOptions) -> Graph {
    let capacity: Vec<NodeCapacity> = map.nodes.iter().map(|n| n.capacity.clone()).collect();
    Graph::build(&map.edges, &capacity, options)
}

/// One fused undirected edge under assembly.
struct MergedEdge {
    a: NodeId,
    b: NodeId,
    rtt_ms: f64,
    samples: u64,
    last_seen_secs: u64,
    weighted_rtt_sum: f64,
}

/// The assembled network graph + the latency query contract. Immutable once built.
pub struct Graph {
    /// Insertion-ordered node ids (observation endpoints first, then capacity-only nodes).
    nodes: Vec<NodeId>,
    node_set: HashSet<NodeId>,
    /// Fused undirected edges, insertion order.
    edges: Vec<MeshEdge>,
    /// Ground-truth direct samples keyed by the order-independent pair key.
    direct_rtt: HashMap<String, f64>,
    /// Neighbour lists over measured edges.
    adjacency: HashMap<NodeId, Vec<(NodeId, f64)>>,
    /// Embedding coordinates per node.
    coords: HashMap<NodeId, Vec<f64>>,
    /// Capacity per node (feasibility substrate).
    capacity: HashMap<NodeId, NodeCapacity>,
    /// O(1) region index per node (the vendor grouping key).
    region_index: HashMap<NodeId, usize>,
    /// Region groups, largest-first, each internally sorted.
    region_groups: Vec<Vec<NodeId>>,
}

impl Graph {
    /// Assemble a graph from directed observations plus capacity records. Bidirectional observations
    /// of the same pair fuse with a sample-weighted mean (JS `Graph.build`).
    pub fn build(observations: &[MeshEdge], capacity: &[NodeCapacity], options: &GraphOptions) -> Graph {
        let mut nodes: Vec<NodeId> = Vec::new();
        let mut node_set: HashSet<NodeId> = HashSet::new();
        let add_node = |nodes: &mut Vec<NodeId>, set: &mut HashSet<NodeId>, id: &str| {
            if !id.is_empty() && !set.contains(id) {
                set.insert(id.to_string());
                nodes.push(id.to_string());
            }
        };

        // Fuse directed observations into undirected edges (insertion-ordered, like the JS Map).
        let mut merged: Vec<MergedEdge> = Vec::new();
        let mut merged_index: HashMap<String, usize> = HashMap::new();
        for o in observations {
            if o.a.is_empty() || o.b.is_empty() || o.a == o.b {
                continue;
            }
            add_node(&mut nodes, &mut node_set, &o.a);
            add_node(&mut nodes, &mut node_set, &o.b);
            let key = pair_key(&o.a, &o.b);
            let (a, b) = if o.a < o.b { (&o.a, &o.b) } else { (&o.b, &o.a) };
            let samples = o.samples.max(1);
            match merged_index.get(&key) {
                None => {
                    merged_index.insert(key, merged.len());
                    merged.push(MergedEdge {
                        a: a.clone(),
                        b: b.clone(),
                        rtt_ms: o.rtt_ms,
                        samples,
                        last_seen_secs: o.last_seen_secs,
                        weighted_rtt_sum: o.rtt_ms * samples as f64,
                    });
                }
                Some(&i) => {
                    let e = &mut merged[i];
                    e.samples += samples;
                    e.weighted_rtt_sum += o.rtt_ms * samples as f64;
                    e.rtt_ms = e.weighted_rtt_sum / e.samples as f64;
                    e.last_seen_secs = e.last_seen_secs.max(o.last_seen_secs);
                }
            }
        }

        for c in capacity {
            add_node(&mut nodes, &mut node_set, &c.node_id);
        }

        let mut edges: Vec<MeshEdge> = Vec::with_capacity(merged.len());
        let mut direct_rtt: HashMap<String, f64> = HashMap::new();
        let mut adjacency: HashMap<NodeId, Vec<(NodeId, f64)>> = HashMap::new();
        for n in &nodes {
            adjacency.insert(n.clone(), Vec::new());
        }
        for e in &merged {
            edges.push(MeshEdge {
                a: e.a.clone(),
                b: e.b.clone(),
                rtt_ms: e.rtt_ms,
                samples: e.samples,
                last_seen_secs: e.last_seen_secs,
            });
            direct_rtt.insert(pair_key(&e.a, &e.b), e.rtt_ms);
            adjacency.get_mut(&e.a).unwrap().push((e.b.clone(), e.rtt_ms));
            adjacency.get_mut(&e.b).unwrap().push((e.a.clone(), e.rtt_ms));
        }

        let coords = embed(&nodes, &edges, &options.embedding);

        let mut capacity_map = HashMap::new();
        for c in capacity {
            capacity_map.insert(c.node_id.clone(), c.clone());
        }

        let region_groups = Self::compute_regions(&nodes, &edges, options.region_threshold_ms);
        let mut region_index = HashMap::new();
        for (i, group) in region_groups.iter().enumerate() {
            for n in group {
                region_index.insert(n.clone(), i);
            }
        }

        Graph {
            nodes,
            node_set,
            edges,
            direct_rtt,
            adjacency,
            coords,
            capacity: capacity_map,
            region_index,
            region_groups,
        }
    }

    /// Union-find latency regions: connected components restricted to measured edges <= threshold.
    /// Largest-first, each internally sorted for determinism. Computed once at build time so
    /// [`region_of`](Self::region_of) is O(1) — the grouping key vendor.rs relies on.
    fn compute_regions(nodes: &[NodeId], edges: &[MeshEdge], threshold_ms: f64) -> Vec<Vec<NodeId>> {
        let mut parent: HashMap<&str, &str> = HashMap::new();
        for n in nodes {
            parent.insert(n, n);
        }

        fn find<'a>(parent: &mut HashMap<&'a str, &'a str>, x: &'a str) -> &'a str {
            let mut root = x;
            while parent[root] != root {
                root = parent[root];
            }
            // Path-compress.
            let mut cur = x;
            while parent[cur] != root {
                let next = parent[cur];
                parent.insert(cur, root);
                cur = next;
            }
            root
        }

        for e in edges {
            if e.rtt_ms <= threshold_ms {
                let rx = find(&mut parent, e.a.as_str());
                let ry = find(&mut parent, e.b.as_str());
                if rx != ry {
                    parent.insert(rx, ry);
                }
            }
        }

        // Group by root, preserving first-seen root order (JS Map semantics).
        let mut groups: Vec<Vec<NodeId>> = Vec::new();
        let mut group_index: HashMap<String, usize> = HashMap::new();
        for n in nodes {
            let root = find(&mut parent, n.as_str()).to_string();
            match group_index.get(&root) {
                Some(&i) => groups[i].push(n.clone()),
                None => {
                    group_index.insert(root, groups.len());
                    groups.push(vec![n.clone()]);
                }
            }
        }
        for g in &mut groups {
            g.sort();
        }
        groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
        groups
    }

    /// All node ids known to the graph (insertion order).
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// True if the node appears anywhere (edge endpoint or capacity row).
    pub fn has(&self, node: &str) -> bool {
        self.node_set.contains(node)
    }

    /// The fused undirected measured edges (the JS `snapshot().edges` substrate).
    pub fn edges(&self) -> &[MeshEdge] {
        &self.edges
    }

    /// Directly measured RTT (ms) between `a` and `b`, order-independent; `None` without a sample.
    pub fn measured_rtt(&self, a: &str, b: &str) -> Option<f64> {
        if a == b {
            return Some(0.0);
        }
        self.direct_rtt.get(&pair_key(a, b)).copied()
    }

    /// Predicted RTT (ms) between any two known nodes via the embedding distance. A direct
    /// measurement, when present, is returned verbatim (ground truth). If the embedding cannot
    /// relate the pair (disconnected components) it falls back to the measured shortest-path cost,
    /// then `INFINITY`.
    pub fn predicted_rtt(&self, a: &str, b: &str) -> f64 {
        if a == b {
            return 0.0;
        }
        if let Some(direct) = self.measured_rtt(a, b) {
            return direct;
        }
        if let (Some(ca), Some(cb)) = (self.coords.get(a), self.coords.get(b)) {
            if self.reachable(a, b) {
                return vec_distance(ca, cb);
            }
        }
        let path = self.shortest_path(a, b);
        if path.is_empty() {
            return f64::INFINITY;
        }
        let mut total = 0.0;
        for i in 0..path.len().saturating_sub(1) {
            total += self.measured_rtt(&path[i], &path[i + 1]).unwrap_or(0.0);
        }
        total
    }

    /// The `k` nodes closest to `node` by predicted RTT, ascending. Excludes `node` and any node
    /// with no finite predicted distance. Deterministic tie-break by id.
    pub fn k_nearest(&self, node: &str, k: usize) -> Vec<NodeId> {
        if !self.node_set.contains(node) || k == 0 {
            return Vec::new();
        }
        let mut ranked: Vec<(&NodeId, f64)> = Vec::new();
        for other in &self.nodes {
            if other == node {
                continue;
            }
            let rtt = self.predicted_rtt(node, other);
            if rtt.is_finite() {
                ranked.push((other, rtt));
            }
        }
        ranked.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| x.0.cmp(y.0)));
        ranked.into_iter().take(k).map(|(id, _)| id.clone()).collect()
    }

    /// Latency regions: components joined by measured edges <= the region threshold. Largest-first.
    pub fn regions(&self) -> &[Vec<NodeId>] {
        &self.region_groups
    }

    /// Stable region index for a node (index into [`regions`](Self::regions)). O(1). `-1` if unknown.
    pub fn region_of(&self, node: &str) -> i64 {
        self.region_index.get(node).map(|&i| i as i64).unwrap_or(-1)
    }

    /// Minimum-RTT path between `a` and `b` over measured edges (Dijkstra). Node sequence including
    /// both endpoints, or empty if unreachable / either unknown.
    pub fn shortest_path(&self, a: &str, b: &str) -> Vec<NodeId> {
        if !self.node_set.contains(a) || !self.node_set.contains(b) {
            return Vec::new();
        }
        if a == b {
            return vec![a.to_string()];
        }

        // Insertion-ordered frontier scan, mirroring the JS Map-based Dijkstra (ties resolve to the
        // first-discovered node, keeping paths deterministic).
        let mut dist_order: Vec<NodeId> = vec![a.to_string()];
        let mut dist: HashMap<NodeId, f64> = HashMap::from([(a.to_string(), 0.0)]);
        let mut prev: HashMap<NodeId, NodeId> = HashMap::new();
        let mut visited: HashSet<NodeId> = HashSet::new();

        loop {
            let mut u: Option<NodeId> = None;
            let mut best = f64::INFINITY;
            for node_id in &dist_order {
                if !visited.contains(node_id) {
                    let d = dist[node_id];
                    if d < best {
                        best = d;
                        u = Some(node_id.clone());
                    }
                }
            }
            let Some(u) = u else { break };
            if u == b {
                break;
            }
            visited.insert(u.clone());
            if let Some(neigh) = self.adjacency.get(&u) {
                for (to, rtt_ms) in neigh {
                    if visited.contains(to) {
                        continue;
                    }
                    let nd = best + rtt_ms;
                    let cur = dist.get(to).copied().unwrap_or(f64::INFINITY);
                    if nd < cur {
                        if !dist.contains_key(to) {
                            dist_order.push(to.clone());
                        }
                        dist.insert(to.clone(), nd);
                        prev.insert(to.clone(), u.clone());
                    }
                }
            }
        }

        if !dist.contains_key(b) {
            return Vec::new();
        }
        let mut path = vec![b.to_string()];
        let mut cur = b.to_string();
        while cur != a {
            let Some(p) = prev.get(&cur) else { return Vec::new() };
            path.push(p.clone());
            cur = p.clone();
        }
        path.reverse();
        path
    }

    /// The capacity row for a node, when the map carried one.
    pub fn capacity_of(&self, node: &str) -> Option<&NodeCapacity> {
        self.capacity.get(node)
    }

    /// The embedding coordinate for a node.
    pub fn coordinate(&self, node: &str) -> Option<&Vec<f64>> {
        self.coords.get(node)
    }

    /// Whether `b` is reachable from `a` over measured edges (gates embedding prediction).
    fn reachable(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let mut seen: HashSet<&str> = HashSet::from([a]);
        let mut stack: Vec<&str> = vec![a];
        while let Some(u) = stack.pop() {
            if let Some(neigh) = self.adjacency.get(u) {
                for (to, _) in neigh {
                    if to == b {
                        return true;
                    }
                    if !seen.contains(to.as_str()) {
                        seen.insert(to);
                        stack.push(to);
                    }
                }
            }
        }
        false
    }
}

impl LatencyView for Graph {
    fn measured_rtt(&self, a: &str, b: &str) -> Option<f64> {
        Graph::measured_rtt(self, a, b)
    }
    fn predicted_rtt(&self, a: &str, b: &str) -> f64 {
        Graph::predicted_rtt(self, a, b)
    }
    fn region_of(&self, node: &str) -> i64 {
        Graph::region_of(self, node)
    }
}

// ----------------------------------------------------------------------------
// Tests — the JS graph.__selftest fixtures translated, plus JS-parity vectors.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-for-bit parity with the JS mulberry32 (reference values generated with node).
    #[test]
    fn mulberry32_matches_js_bit_for_bit() {
        let mut r = Mulberry32::new(12345);
        let expect = [
            0.97972826776094735,
            0.30675226449966431,
            0.48420542152598500,
            0.81793441250920296,
            0.50942836934700608,
        ];
        for e in expect {
            assert_eq!(r.next(), e, "mulberry32(12345) stream must match JS exactly");
        }
        let mut r0 = Mulberry32::new(0);
        let expect0 = [0.26642920868471265, 0.00032974570058286190, 0.22327202744781971];
        for e in expect0 {
            assert_eq!(r0.next(), e, "mulberry32(0) stream must match JS exactly");
        }
    }

    fn obs(a: &str, b: &str, rtt_ms: f64, samples: u64, last_seen_secs: u64) -> MeshEdge {
        MeshEdge { a: a.into(), b: b.into(), rtt_ms, samples, last_seen_secs }
    }

    fn cap(node_id: &str, cpu_cores: u32, mem_mb: u64, running_jobs: u32, last_seen_secs: u64, tags: &[&str]) -> NodeCapacity {
        NodeCapacity {
            node_id: node_id.into(),
            cpu_cores,
            mem_mb,
            running_jobs,
            last_seen_secs,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ask_base_units: None,
        }
    }

    /// The JS graph.__selftest fixture: two tight regions (US, EU) bridged by one transatlantic
    /// link, plus an atlas-only isolated node. Directed observations in the same order the JS
    /// netgraphByOrigin flattens to, so the embedding is bit-comparable.
    fn fixture() -> Graph {
        let observations = vec![
            obs("us-a", "us-b", 5.0, 10, 1000),
            obs("us-a", "eu-a", 90.0, 8, 1000),
            obs("us-b", "us-a", 7.0, 2, 1100), // disagrees; fewer samples
            obs("us-b", "us-c", 4.0, 6, 1100),
            obs("eu-a", "eu-b", 6.0, 5, 1200),
            obs("eu-a", "eu-c", 40.0, 3, 1200), // > threshold: same region only via eu-b
            obs("eu-b", "eu-c", 7.0, 5, 1250),
        ];
        let capacity = vec![
            cap("us-a", 8, 16000, 1, 1000, &["docker", "us"]),
            cap("us-b", 4, 8000, 0, 1100, &["docker", "us"]),
            cap("us-c", 16, 64000, 2, 1100, &["docker", "gpu", "us"]),
            cap("eu-a", 8, 16000, 0, 1200, &["docker", "eu"]),
            cap("eu-b", 4, 8000, 1, 1200, &["docker", "eu"]),
            cap("eu-c", 8, 32000, 0, 1250, &["docker", "eu"]),
            cap("lone-x", 32, 128000, 0, 1300, &["docker", "gpu"]),
        ];
        let options = GraphOptions {
            region_threshold_ms: 30.0,
            embedding: EmbeddingOptions { iterations: 600, seed: 7, ..Default::default() },
        };
        Graph::build(&observations, &capacity, &options)
    }

    #[test]
    fn nodes_and_has() {
        let g = fixture();
        assert_eq!(g.nodes().len(), 7);
        assert!(g.has("lone-x"), "atlas-only node should be present");
        assert!(!g.has("nope"), "unknown node should be absent");
    }

    #[test]
    fn edge_fusion_sample_weighted_mean() {
        let g = fixture();
        // us-a<->us-b: 5ms*10 + 7ms*2 = 64 over 12 samples = 5.333... (weighted toward the 10-sample side).
        let rtt_ab = g.measured_rtt("us-a", "us-b").expect("us-a<->us-b must have a measured edge");
        assert!((rtt_ab - 64.0 / 12.0).abs() < 1e-9, "fused RTT should be 64/12, got {rtt_ab}");
        assert_eq!(g.measured_rtt("us-b", "us-a"), Some(rtt_ab), "measured_rtt must be order-independent");
        assert_eq!(g.measured_rtt("us-a", "us-c"), None, "us-a<->us-c is not a direct edge");
        assert_eq!(g.measured_rtt("us-a", "us-a"), Some(0.0), "self RTT is 0");
    }

    #[test]
    fn predicted_rtt_matches_js_embedding() {
        let g = fixture();
        let p = g.predicted_rtt("us-c", "eu-c");
        assert!(p.is_finite() && p > 0.0, "predicted RTT across the bridge must be finite and positive");
        // JS reference (iterations 600, seed 7): predictedRtt("us-c","eu-c") = 51.257514135458280.
        assert!((p - 51.25751413545828).abs() < 1e-9, "embedding must reproduce the JS layout, got {p}");
        let p2 = g.predicted_rtt("us-a", "eu-b");
        assert!((p2 - 75.685146243678247).abs() < 1e-9, "second JS reference distance, got {p2}");
        // JS reference coordinate for us-a.
        let c = g.coordinate("us-a").unwrap();
        assert!((c[0] - -31.959566242480143).abs() < 1e-9 && (c[1] - -28.453558538548794).abs() < 1e-9);
        // Direct measurement is returned verbatim as ground truth.
        assert_eq!(g.predicted_rtt("us-a", "us-b"), g.measured_rtt("us-a", "us-b").unwrap());
        // lone-x is isolated => no path, no shared component => Infinity.
        assert!(!g.predicted_rtt("us-a", "lone-x").is_finite(), "isolated node predicted RTT must be Infinity");
    }

    #[test]
    fn k_nearest_prefers_low_rtt_same_region() {
        let g = fixture();
        let near = g.k_nearest("us-a", 3);
        // JS reference order for this exact fixture: us-b, us-c, eu-c.
        assert_eq!(near, vec!["us-b", "us-c", "eu-c"], "k_nearest must match the JS ranking");
        assert!(!near.contains(&"us-a".to_string()), "k_nearest must exclude the query node");
        assert!(!near.contains(&"lone-x".to_string()), "k_nearest must exclude unreachable nodes");
        // Every US peer of us-a must be predicted nearer than every EU node.
        for us in ["us-b", "us-c"] {
            for eu in ["eu-a", "eu-b", "eu-c"] {
                assert!(
                    g.predicted_rtt("us-a", us) < g.predicted_rtt("us-a", eu),
                    "same-region {us} must be nearer than cross-region {eu}"
                );
            }
        }
        assert!(g.k_nearest("nope", 3).is_empty(), "unknown query node yields nothing");
        assert!(g.k_nearest("us-a", 0).is_empty(), "k=0 yields nothing");
    }

    #[test]
    fn regions_union_find_clusters_us_eu_apart() {
        let g = fixture();
        // JS reference: [["eu-a","eu-b","eu-c"],["us-a","us-b","us-c"],["lone-x"]] (largest-first,
        // size ties broken by first id — "eu-a" < "us-a").
        assert_eq!(
            g.regions(),
            &[
                vec!["eu-a".to_string(), "eu-b".into(), "eu-c".into()],
                vec!["us-a".to_string(), "us-b".into(), "us-c".into()],
                vec!["lone-x".to_string()],
            ]
        );
        let reg_us = g.region_of("us-a");
        let reg_eu = g.region_of("eu-a");
        let reg_lone = g.region_of("lone-x");
        assert!(reg_us >= 0 && reg_eu >= 0 && reg_lone >= 0, "every node must have a region index");
        assert_ne!(reg_us, reg_eu, "US and EU must be different latency regions");
        assert!(reg_lone != reg_us && reg_lone != reg_eu, "lone-x must be its own region");
        // region_of is consistent with regions(); eu-c joins EU via eu-b (7ms) even though
        // eu-a<->eu-c is 40ms (> threshold): union-find transitivity.
        assert!(g.regions()[reg_us as usize].contains(&"us-a".to_string()));
        assert_eq!(g.region_of("nope"), -1, "unknown node region_of must be -1");
    }

    #[test]
    fn shortest_path_routes_across_the_bridge() {
        let g = fixture();
        let path = g.shortest_path("us-c", "eu-c");
        // JS reference: us-c -> us-b -> us-a -> eu-a -> eu-b -> eu-c.
        assert_eq!(path, vec!["us-c", "us-b", "us-a", "eu-a", "eu-b", "eu-c"]);
        assert!(g.shortest_path("us-a", "lone-x").is_empty(), "no path to an isolated node");
        assert_eq!(g.shortest_path("us-a", "us-a"), vec!["us-a"], "self path is the singleton");
        assert!(g.shortest_path("nope", "us-a").is_empty(), "unknown endpoint yields empty");
    }

    #[test]
    fn capacity_folds_in() {
        let g = fixture();
        let c = g.capacity_of("us-c").expect("us-c capacity must be present");
        assert_eq!(c.cpu_cores, 16);
        assert_eq!(c.mem_mb, 64000);
        assert!(c.tags.iter().any(|t| t == "gpu"), "capacity tags must be carried for tag requirements");
        assert_eq!(g.capacity_of("lone-x").unwrap().cpu_cores, 32, "atlas-only node still carries capacity");
    }

    #[test]
    fn embed_is_deterministic_per_seed() {
        let nodes: Vec<NodeId> = vec!["a".into(), "b".into(), "c".into()];
        let edges = vec![obs("a", "b", 10.0, 5, 0), obs("b", "c", 20.0, 5, 0)];
        let o7 = EmbeddingOptions { seed: 7, ..Default::default() };
        let c1 = embed(&nodes, &edges, &o7);
        let c2 = embed(&nodes, &edges, &o7);
        assert_eq!(c1, c2, "same (input, seed) must reproduce identical coordinates");
        let o8 = EmbeddingOptions { seed: 8, ..Default::default() };
        let c3 = embed(&nodes, &edges, &o8);
        assert_ne!(c1, c3, "a different seed lays out differently");
        // Isolated nodes still get a coordinate.
        let c4 = embed(&["x".to_string()], &[], &EmbeddingOptions::default());
        assert!(c4.contains_key("x"));
    }

    #[test]
    fn build_graph_reads_the_fabric_map() {
        let map = FabricMap {
            nodes: vec![crate::api::FabricNode {
                node_id: "a".into(),
                capacity: cap("a", 4, 8000, 0, 0, &["docker"]),
                profile: None,
                history: None,
            }],
            edges: vec![obs("a", "b", 12.0, 3, 0)],
            ..Default::default()
        };
        let g = build_graph(&map, &GraphOptions::default());
        assert!(g.has("a") && g.has("b"), "graph covers edge endpoints and capacity rows");
        assert_eq!(g.measured_rtt("a", "b"), Some(12.0));
        assert_eq!(g.capacity_of("a").unwrap().cpu_cores, 4);
    }
}
