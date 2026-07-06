//! Split health probes: `/healthz` (liveness) + `/readyz` (readiness, with a
//! drain bit), exposed over axum. The same state also implements
//! [`ProbeSink`], so the readiness/liveness can be served identically over
//! JSON-RPC (`Lifecycle.Status` / `Lifecycle.Health`) with no HTTP framework.

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::get,
};

use crate::{DependencyCheck, DrainState, HealthStatus, ProbeSink, ReadyStatus};

type DepChecker = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// Shared state for the probe routes + the [`ProbeSink`] impl. Clone it cheaply.
#[derive(Clone)]
pub struct ProbeState {
    inner: Arc<ProbeInner>,
}

struct ProbeInner {
    version: String,
    start: Instant,
    drain_state: std::sync::Mutex<DrainState>,
    generation: std::sync::Mutex<Option<u64>>,
    deps: std::sync::Mutex<Vec<(String, DepChecker)>>,
}

impl ProbeState {
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(ProbeInner {
                version: version.into(),
                start: Instant::now(),
                drain_state: std::sync::Mutex::new(DrainState::Active),
                generation: std::sync::Mutex::new(None),
                deps: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Register a readiness dependency.
    pub fn add_dependency<F>(&self, name: impl Into<String>, check: F)
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        self.inner
            .deps
            .lock()
            .unwrap()
            .push((name.into(), Arc::new(check)));
    }

    /// Set the drain state (Active / Draining / Reloading).
    pub fn set_drain_state(&self, state: DrainState) {
        *self.inner.drain_state.lock().unwrap() = state;
    }

    /// Record the deployment generation.
    pub fn set_generation(&self, generation: Option<u64>) {
        *self.inner.generation.lock().unwrap() = generation;
    }
}

#[async_trait]
impl ProbeSink for ProbeState {
    async fn ready(&self) -> ReadyStatus {
        let drain_state = self
            .inner
            .drain_state
            .lock()
            .ok()
            .map_or(DrainState::Draining, |g| *g);
        let generation = self.inner.generation.lock().ok().and_then(|g| *g);
        let draining = matches!(drain_state, DrainState::Draining | DrainState::Reloading);
        if let Ok(deps) = self.inner.deps.lock() {
            let mut dependencies = Vec::with_capacity(deps.len());
            let mut all_ok = true;
            for (name, check) in deps.iter() {
                let ok = check();
                if !ok {
                    all_ok = false;
                }
                dependencies.push(DependencyCheck {
                    name: name.clone(),
                    ok,
                    detail: if ok { None } else { Some("unhealthy".into()) },
                });
            }
            let ready = !draining && all_ok;
            ReadyStatus {
                ready,
                draining,
                dependencies,
                generation,
            }
        } else {
            ReadyStatus {
                ready: false,
                draining: true,
                dependencies: vec![DependencyCheck {
                    name: "internal_lock".into(),
                    ok: false,
                    detail: Some("registry mutex poisoned".into()),
                }],
                generation: None,
            }
        }
    }

    async fn health(&self) -> HealthStatus {
        HealthStatus {
            alive: true,
            pid: std::process::id(),
            uptime_secs: self.inner.start.elapsed().as_secs(),
            version: self.inner.version.clone(),
        }
    }
}

/// Build a `Router<()>` exposing `GET /healthz` and `GET /readyz`.
pub fn probe_router(state: ProbeState) -> Router<()> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

async fn healthz(State(state): State<ProbeState>) -> Json<HealthStatus> {
    Json(state.health().await)
}

async fn readyz(State(state): State<ProbeState>) -> Response {
    let status = state.ready().await;
    let code = if status.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(status)).into_response()
}
