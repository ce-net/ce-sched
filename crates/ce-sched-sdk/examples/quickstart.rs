//! Quickstart: read the Fabric Map the local ce-sched daemon plans against, then ask it for a
//! 1-host placement plan (127.0.0.1:8856; the daemon enriches its map from ce-bench at :8855).
//!
//!     cargo run -p ce-sched-sdk --example quickstart

use ce_sched_sdk::{JobSpec, SchedClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let c = SchedClient::local();
    let map = c.map().await?;
    println!(
        "map: {} node(s), {} edge(s), beacon height {}",
        map.nodes.len(),
        map.edges.len(),
        map.beacon.height
    );

    let spec = JobSpec { k: Some(1), cpu_cores: 1, mem_mb: 256, allow_self: true, ..Default::default() };
    let plan = c.plan(&spec).await?;
    println!(
        "plan: {}/{} host(s), shortfall={}",
        plan.targets.len(),
        plan.requested_k,
        plan.shortfall
    );
    for t in &plan.targets {
        println!("  -> {}… score={:.4}", &t.node_id[..12.min(t.node_id.len())], t.score);
    }
    Ok(())
}
