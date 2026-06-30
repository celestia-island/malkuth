//! Layer 1 — split health probes.
//!
//! - `GET /healthz` → [`HealthStatus`] (liveness): "the process can answer".
//! - `GET /readyz`  → [`ReadyStatus`] (readiness): "can serve", with a
//!   `draining` bit. Returns `503` while draining or while any registered
//!   dependency is unhealthy.
//!
//! The `draining` bit is the central rolling-update signal: a load balancer
//! or orchestrator routes new traffic only to instances whose
//! `ready && !draining`.

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use tokio::sync::RwLock;

use crate::{DependencyCheck, HealthStatus, ReadyStatus};

/// A dependency checker is a plain sync predicate. It should be cheap and
/// non-blocking (e.g. read an atomic, ping a cached connection).
type DepChecker = Arc<dyn Fn() -> bool + Send + Sync>;

struct DepEntry {
    name: String,
    check: DepChecker,
}

/// Shared state for the probe routes. Clone it cheaply.
#[derive(Clone)]
pub struct ProbeState {
    inner: Arc<ProbeInner>,
}

struct ProbeInner {
    version: String,
    start: Instant,
    draining: RwLock<bool>,
    generation: RwLock<Option<u64>>,
    deps: RwLock<Vec<DepEntry>>,
}

impl ProbeState {
    /// Create a probe state with the build `version` string.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(ProbeInner {
                version: version.into(),
                start: Instant::now(),
                draining: RwLock::new(false),
                generation: RwLock::new(None),
                deps: RwLock::new(Vec::new()),
            }),
        }
    }

    /// Register a readiness dependency: `name` is reported in
    /// [`ReadyStatus::dependencies`]; `check` returns true when healthy.
    pub async fn add_dependency<F>(&self, name: impl Into<String>, check: F)
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        self.inner.deps.write().await.push(DepEntry {
            name: name.into(),
            check: Arc::new(check),
        });
    }

    /// Mark this instance as draining (or clear the bit).
    pub async fn set_draining(&self, draining: bool) {
        *self.inner.draining.write().await = draining;
    }

    /// Record the deployment generation (rolling-update bookkeeping).
    pub async fn set_generation(&self, generation: Option<u64>) {
        *self.inner.generation.write().await = generation;
    }

    /// Compute the readiness status by invoking all dependency checks.
    pub async fn ready_status(&self) -> ReadyStatus {
        let draining = *self.inner.draining.read().await;
        let generation = *self.inner.generation.read().await;
        let deps = self.inner.deps.read().await;
        let mut dependencies = Vec::with_capacity(deps.len());
        let mut all_ok = true;
        for entry in deps.iter() {
            let ok = (entry.check)();
            if !ok {
                all_ok = false;
            }
            dependencies.push(DependencyCheck {
                name: entry.name.clone(),
                ok,
                detail: if ok {
                    None
                } else {
                    Some("unhealthy".to_string())
                },
            });
        }
        let ready = !draining && all_ok;
        ReadyStatus {
            ready,
            draining,
            dependencies,
            generation,
        }
    }

    fn health_status(&self) -> HealthStatus {
        HealthStatus {
            alive: true,
            pid: std::process::id(),
            uptime_secs: self.inner.start.elapsed().as_secs(),
            version: self.inner.version.clone(),
        }
    }
}

/// Build a `Router<()>` exposing `GET /healthz` and `GET /readyz`, ready to
/// `.merge()` into a larger app.
pub fn probe_router(state: ProbeState) -> Router<()> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

async fn healthz(State(state): State<ProbeState>) -> Json<HealthStatus> {
    Json(state.health_status())
}

async fn readyz(State(state): State<ProbeState>) -> Response {
    let status = state.ready_status().await;
    let code = if status.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(status)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_when_not_draining_and_deps_ok() {
        let s = ProbeState::new("0.1.0");
        s.add_dependency("db", || true).await;
        let r = s.ready_status().await;
        assert!(r.ready);
        assert!(!r.draining);
    }

    #[tokio::test]
    async fn not_ready_when_draining() {
        let s = ProbeState::new("0.1.0");
        s.set_draining(true).await;
        let r = s.ready_status().await;
        assert!(!r.ready);
        assert!(r.draining);
    }

    #[tokio::test]
    async fn not_ready_when_dependency_unhealthy() {
        let s = ProbeState::new("0.1.0");
        s.add_dependency("db", || false).await;
        let r = s.ready_status().await;
        assert!(!r.ready);
    }
}
