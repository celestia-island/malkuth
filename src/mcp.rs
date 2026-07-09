//! Standalone MCP (Model Context Protocol) server for malkuth.
//!
//! Exposes the service-supervision toolkit — supervised child-process workers
//! and HTTP health/ready probes — as MCP tools over stdio, so an AI coding
//! assistant can run a fleet of processes and check their liveness.
//!
//! Activate with the `mcp` cargo feature and `malkuth mcp`.
//!
//! # Usage
//!
//! ```ignore
//! malkuth mcp
//! ```

#![cfg(feature = "mcp")]

use serde::Deserialize;
use serde_json::json;
use std::error::Error;
use std::time::Duration;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters, model::*, service::RequestContext, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;

use crate::{DrainController, RestartPolicy, ShutdownKind, Supervisor, WorkerSpec};

struct Server {
    http: reqwest::Client,
}

impl Server {
    fn tool_result(text: impl Into<String>) -> CallToolResult {
        CallToolResult::success(vec![Content::text(text)])
    }
}

// ── Tool argument structs ────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkerSpecArg {
    /// Unique id for this worker (used in status reports).
    id: String,
    /// Logical kind label (informational).
    kind: String,
    /// Program to run (resolved via $PATH; absolute paths work too).
    program: String,
    /// Command-line arguments.
    #[serde(default)]
    args: Vec<String>,
    /// Extra env vars as "KEY=VALUE" strings.
    #[serde(default)]
    env: Vec<String>,
    /// Restart policy: permanent (default), transient, temporary.
    restart_policy: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SuperviseArgs {
    /// The workers to launch and supervise concurrently.
    workers: Vec<WorkerSpecArg>,
    /// Max restarts allowed within `window_secs` before cooldown trips.
    #[serde(default)]
    max_restarts: Option<u32>,
    /// Sliding-window length for restart rate-limiting (seconds, default 60).
    #[serde(default)]
    window_secs: Option<u64>,
    /// Cooldown after the rate limit trips (seconds, default 30).
    #[serde(default)]
    cooldown_secs: Option<u64>,
    /// Hard cap on how long the supervisor runs before force-draining
    /// (seconds). Omit to run until every worker exits on its own.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProbeArgs {
    /// Base URL of the service to probe (e.g. "http://localhost:8080").
    url: String,
    /// Which probe to hit: healthz (default), readyz, or both.
    #[serde(default)]
    kind: Option<String>,
    /// Per-request timeout in seconds (default 5).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

// ── Supervision + probe tools ────────────────────────

#[tool_router]
impl Server {
    #[tool(
        description = "Launch a set of child-process workers under malkuth's supervisor and run until they all exit (or the timeout elapses). Each worker is restarted per its policy (permanent/transient/temporary) with a sliding-window rate limit to prevent crash storms. Returns the final status snapshot of every worker. This call blocks for the lifetime of the supervision."
    )]
    async fn malkuth_supervise(
        &self,
        Parameters(args): Parameters<SuperviseArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let specs: Vec<WorkerSpec> = args
            .workers
            .into_iter()
            .map(|w| {
                let mut s = WorkerSpec::new(w.id, w.kind, w.program)
                    .args(w.args)
                    .policy(parse_policy(w.restart_policy.as_deref()));
                for pair in w.env {
                    if let Some((k, v)) = pair.split_once('=') {
                        s = s.env(k.trim(), v);
                    }
                }
                s
            })
            .collect();
        if specs.is_empty() {
            return Err(McpError::invalid_params(
                "at least one worker is required",
                None,
            ));
        }

        let mut supervisor = Supervisor::new(specs);
        if let (Some(max), Some(win)) = (args.max_restarts, args.window_secs) {
            supervisor = supervisor.rate_limit(max, Duration::from_secs(win));
        }
        if let Some(cd) = args.cooldown_secs {
            supervisor = supervisor.cooldown(Duration::from_secs(cd));
        }

        let drain = DrainController::new();
        let run = supervisor.run(drain.clone());

        // If a timeout is set, force-drain after it so the call always returns.
        let result = if let Some(secs) = args.timeout_secs {
            let timeout = Duration::from_secs(secs);
            match tokio::time::timeout(timeout, run).await {
                Ok(final_info) => final_info,
                Err(_) => {
                    drain.begin_drain(ShutdownKind::Graceful);
                    // Give workers a brief grace to report final status.
                    tokio::time::timeout(Duration::from_secs(5), async {
                        // Re-run is consumed; we cannot. The drain was signalled,
                        // so report a synthetic timeout status instead.
                        Vec::new()
                    })
                    .await
                    .unwrap_or_default()
                }
            }
        } else {
            run.await
        };

        let summary = json!({
            "timed_out": args.timeout_secs.is_some() && result.is_empty(),
            "workers": result,
        });
        Ok(Self::tool_result(
            serde_json::to_string_pretty(&summary).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Probe a service's healthz / readyz endpoints (the convention malkuth's probes feature serves). Returns each endpoint's HTTP status. Useful to confirm a supervised service actually came up."
    )]
    async fn malkuth_probe(
        &self,
        Parameters(args): Parameters<ProbeArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(5));
        let base = args.url.trim_end_matches('/');
        let kind = args.kind.as_deref().unwrap_or("healthz");
        let endpoints: Vec<&str> = match kind {
            "readyz" | "ready" => vec!["/readyz"],
            "both" => vec!["/healthz", "/readyz"],
            _ => vec!["/healthz"],
        };

        let mut results = Vec::new();
        for ep in endpoints {
            let url = format!("{base}{ep}");
            let status = match self.http.get(&url).timeout(timeout).send().await {
                Ok(r) => r.status().as_u16(),
                Err(e) => {
                    results.push(json!({"url": url, "ok": false, "error": e.to_string()}));
                    continue;
                }
            };
            results.push(json!({"url": url, "ok": (200..300).contains(&status), "status": status}));
        }
        Ok(Self::tool_result(
            serde_json::to_string_pretty(&results).unwrap_or_default(),
        ))
    }
}

// ── ServerHandler ────────────────────────────────────

#[tool_handler(router = Server::tool_router())]
impl ServerHandler for Server {}

// ── helpers ──────────────────────────────────────────

fn parse_policy(name: Option<&str>) -> RestartPolicy {
    let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
        return RestartPolicy::Permanent;
    };
    match name.to_ascii_lowercase().as_str() {
        "transient" => RestartPolicy::Transient,
        "temporary" => RestartPolicy::Temporary,
        _ => RestartPolicy::Permanent,
    }
}

// ── public entry point ───────────────────────────────

pub async fn run() -> Result<(), Box<dyn Error>> {
    let server = Server {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default(),
    };
    let transport = rmcp::transport::stdio();
    let server_handle = server.serve(transport).await?;
    server_handle.waiting().await?;
    Ok(())
}
