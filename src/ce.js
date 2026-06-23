/**
 * Zero-dependency CE HTTP client for @ce-net/sched.
 *
 * Reads the node's placement read-substrate (`/netgraph`, `/atlas`, `/history`, `/beacon`) and
 * issues dispatch calls (`/mesh-deploy`, `/mesh-kill`). Web-standard `fetch` only — runs in Node
 * 18+ and the browser unchanged. No retries/backoff baked in (placement is fast and re-run often);
 * callers that want resilience wrap these.
 *
 * Money on the wire is base-unit decimal strings; this client passes them through verbatim and
 * never coerces them to Number (they exceed 2^53). The scorer parses to BigInt where needed.
 *
 * @module ce
 */

/** @typedef {import("./types.js").RawNetGraphEdge} RawNetGraphEdge */
/** @typedef {import("./types.js").RawAtlasEntry} RawAtlasEntry */

const DEFAULT_BASE = "http://localhost:8844";

/** A thin CE node client bound to one base URL. */
export class CeClient {
  /**
   * @param {string} [baseUrl] node HTTP API base, default http://localhost:8844
   * @param {{ fetch?: typeof fetch, timeoutMs?: number, headers?: Record<string,string> }} [options]
   */
  constructor(baseUrl = DEFAULT_BASE, options = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this._fetch = options.fetch ?? globalThis.fetch;
    if (typeof this._fetch !== "function") {
      throw new Error("CeClient: no fetch available; pass options.fetch on this runtime");
    }
    this.timeoutMs = options.timeoutMs ?? 8000;
    this.headers = options.headers ?? {};
  }

  /**
   * Internal: fetch a path and decode JSON, with a timeout and a useful error on non-2xx.
   * @template T
   * @param {string} path
   * @param {RequestInit} [init]
   * @returns {Promise<T>}
   */
  async _json(path, init = {}) {
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
        throw new Error(`CE ${init.method ?? "GET"} ${path} -> ${res.status}: ${text.slice(0, 300)}`);
      }
      return text ? JSON.parse(text) : /** @type {T} */ (undefined);
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * `GET /status` — this node's identity + chain tip ({ node_id, height, balance, ... }). Used by the
   * `planPlacement` facade to auto-resolve the payer (the latency origin) when the caller omits it.
   * Balances are decimal STRINGS; returned verbatim.
   * @returns {Promise<{ node_id?: string, height?: number, balance?: string }>}
   */
  async status() {
    return this._json("/status");
  }

  /**
   * `GET /netgraph` — the measured RTT edges from THIS node to each directly connected peer.
   * The origin of every edge is this node's own id; placer.buildGraph keys observations by the
   * base URL / supplied origin. Returns the raw snake_case rows.
   * @returns {Promise<RawNetGraphEdge[]>}
   */
  async netgraph() {
    const rows = await this._json("/netgraph");
    return Array.isArray(rows) ? rows : [];
  }

  /**
   * `GET /atlas` — the latest capacity snapshot per peer (cpu/mem/jobs/last_seen/tags, plus the
   * future signed `profile`). Raw snake_case rows.
   * @returns {Promise<RawAtlasEntry[]>}
   */
  async atlas() {
    const rows = await this._json("/atlas");
    return Array.isArray(rows) ? rows : [];
  }

  /**
   * `GET /history/:node_id` — on-chain interaction facts (the reputation substrate). Amounts are
   * base-unit STRINGS; returned verbatim (do not coerce). Returns null on a 400 (bad id) so callers
   * can treat "no reputation" uniformly.
   * @param {string} nodeId 64-hex node id
   * @returns {Promise<object|null>}
   */
  async history(nodeId) {
    try {
      return await this._json(`/history/${nodeId}`);
    } catch (err) {
      if (String(err).includes("-> 400")) return null;
      throw err;
    }
  }

  /**
   * Fetch `/history` for many node ids concurrently, tolerating per-node failures (a failed lookup
   * yields null). Returns a Map keyed by node id.
   * @param {string[]} nodeIds
   * @returns {Promise<Map<string, object|null>>}
   */
  async histories(nodeIds) {
    const out = new Map();
    const results = await Promise.allSettled(nodeIds.map((id) => this.history(id)));
    nodeIds.forEach((id, i) => {
      const r = results[i];
      out.set(id, r.status === "fulfilled" ? r.value : null);
    });
    return out;
  }

  /**
   * `GET /beacon` — verifiable public randomness from the PoW tip ({ height, hash }). Used to seed
   * BFT-safe, auditable selection (placement-design.md §6).
   * @returns {Promise<{ height: number, hash: string }>}
   */
  async beacon() {
    return this._json("/beacon");
  }

  /**
   * `POST /mesh-deploy` — dispatch a long-running cell to a specific host. `spec.bid` is a base-unit
   * STRING. Returns `{ job_id }`. This is the action the caller takes AFTER `ce-sched` returns a
   * PlacementPlan; ce-sched itself never dispatches.
   * @param {{ node_id:string, image:string, cmd:string[], cpu_cores:number, mem_mb:number,
   *           duration_secs:number, bid:string, hint_multiaddr?:string, grant?:(string|null) }} spec
   * @returns {Promise<{ job_id: string }>}
   */
  async meshDeploy(spec) {
    if (spec && typeof spec.bid !== "string") {
      throw new Error("meshDeploy: bid must be a base-unit string, not a number");
    }
    return this._json("/mesh-deploy", { method: "POST", body: JSON.stringify(spec) });
  }

  /**
   * `POST /mesh-kill` — stop a mesh-deployed job on a host. 204 No Content on success.
   * @param {{ node_id:string, job_id:string, grant?:(string|null) }} spec
   * @returns {Promise<void>}
   */
  async meshKill(spec) {
    await this._json("/mesh-kill", { method: "POST", body: JSON.stringify(spec) });
  }
}

/** Convenience factory mirroring the class constructor. */
export function ceClient(baseUrl, options) {
  return new CeClient(baseUrl, options);
}
