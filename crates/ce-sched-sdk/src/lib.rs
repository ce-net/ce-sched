//! # ce-sched-sdk — typed Rust client for the ce-sched daemon.
//!
//! A thin async client over the per-machine ce-sched daemon's HTTP API ([`SchedClient`]), mirroring
//! how [`ce_rs::CeClient`](https://docs.rs/ce-rs) wraps the node. It defaults to the local daemon at
//! `http://127.0.0.1:8856` so any tool on the machine finds it deterministically.
//!
//! Two ways to plan:
//!
//! 1. **Via the daemon** — [`SchedClient::plan`] / [`SchedClient::place`] / [`SchedClient::map`].
//!    The daemon owns the assembled [`FabricMap`] (from the local ce-bench daemon) and the dispatch
//!    path, so the SDK call is one HTTP hop.
//! 2. **Pure, no daemon** — the planner is re-exported from [`ce_sched_core`]: call
//!    [`plan_pure`] with a [`JobSpec`] + a [`FabricMap`] you already have. Same code the daemon runs.
//!
//! ```no_run
//! use ce_sched_sdk::{SchedClient, JobSpec};
//! # async fn demo() -> anyhow::Result<()> {
//! let sched = SchedClient::local();
//! let spec = JobSpec { cpu_cores: 1, mem_mb: 256, ..Default::default() };
//! let plan = sched.plan(&spec).await?;
//! println!("{} targets", plan.targets.len());
//! # Ok(()) }
//! ```

use anyhow::{anyhow, Result};

// Re-export the whole wire vocabulary + the pure planner so callers depend on just this crate.
pub use ce_sched_core::api::{
    DispatchResult, FabricMap, JobSpec, PlaceRequest, PlacementPlan,
};
pub use ce_sched_core::{api, placer, plan as plan_pure, PlanError};

/// Default local ce-sched daemon HTTP API base URL.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8856";

/// Async client for a ce-sched daemon's HTTP API.
#[derive(Debug, Clone)]
pub struct SchedClient {
    base: String,
    http: reqwest::Client,
}

impl SchedClient {
    /// Client for a daemon at `base_url` (e.g. `http://127.0.0.1:8856`).
    pub fn new(base_url: impl Into<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        SchedClient { base, http: reqwest::Client::new() }
    }

    /// Client for the local ce-sched daemon on the default port (8856).
    pub fn local() -> Self {
        Self::new(DEFAULT_BASE_URL)
    }

    /// The daemon API base URL this client targets (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `GET /health` — true if the daemon is up.
    pub async fn health(&self) -> Result<bool> {
        Ok(self.http.get(self.url("/health")).send().await?.status().is_success())
    }

    /// `GET /map` — the assembled [`FabricMap`] the daemon plans against.
    pub async fn map(&self) -> Result<FabricMap> {
        json(self.http.get(self.url("/map")).send().await?).await
    }

    /// `POST /plan` — submit a [`JobSpec`], get a [`PlacementPlan`]. The daemon auto-fills the payer
    /// (latency origin) from the local node when `spec.payer` is `None`.
    pub async fn plan(&self, spec: &JobSpec) -> Result<PlacementPlan> {
        json(self.http.post(self.url("/plan")).json(spec).send().await?).await
    }

    /// `POST /place` — dispatch a [`PlacementPlan`]'s targets via the node's `/mesh-deploy`, returning
    /// per-host [`DispatchResult`]s.
    pub async fn place(&self, req: &PlaceRequest) -> Result<DispatchResult> {
        json(self.http.post(self.url("/place")).json(req).send().await?).await
    }
}

/// Deserialize a successful JSON response, or surface an error with status + body.
async fn json<T: for<'de> serde::Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("ce-sched API {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| anyhow!("decode {status} body: {e}: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_trimmed() {
        let c = SchedClient::new("http://example.com:8856/");
        assert_eq!(c.base_url(), "http://example.com:8856");
        assert_eq!(c.url("/plan"), "http://example.com:8856/plan");
    }

    #[test]
    fn local_uses_default_base() {
        assert_eq!(SchedClient::local().base_url(), DEFAULT_BASE_URL);
    }

    #[test]
    fn pure_planner_is_reexported() {
        // The same planner the daemon runs is callable with no daemon (here the stub precondition).
        let map = FabricMap::default();
        let spec = JobSpec { cpu_cores: 1, mem_mb: 256, ..Default::default() };
        assert_eq!(plan_pure(&spec, &map).unwrap_err(), PlanError::MissingPayer);
    }
}
