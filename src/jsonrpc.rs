//! JSON-RPC 2.0 envelope types, error codes and (de)serialization.
//!
//! Wire framing (NDJSON) lives in `crate::codec::FramedConn`; this module only
//! deals with the request/response/error shapes and the handler dispatch.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

// ═══════════════════════════════════════════════════════════════
// Id / Request / Response
// ═══════════════════════════════════════════════════════════════

/// A JSON-RPC request/response id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    /// `null` id.
    Null,
    /// Numeric id.
    Num(u64),
    /// String id.
    Str(String),
}

impl Id {
    fn as_value(&self) -> Value {
        match self {
            Id::Null => Value::Null,
            Id::Num(n) => Value::from(*n),
            Id::Str(s) => Value::String(s.clone()),
        }
    }
}

impl Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.as_value().serialize(ser)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(de)?;
        match v {
            Value::Null => Ok(Id::Null),
            Value::Number(n) => n
                .as_u64()
                .map(Id::Num)
                .ok_or_else(|| serde::de::Error::custom("numeric id must be a u64")),
            Value::String(s) => Ok(Id::Str(s)),
            _ => Err(serde::de::Error::custom(
                "id must be null, number or string",
            )),
        }
    }
}

/// A parsed JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Short error message.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Standard "method not found" (-32601).
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }
    /// Standard "invalid params" (-32602).
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
            data: None,
        }
    }
    /// Standard "parse error" (-32700).
    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: msg.into(),
            data: None,
        }
    }
    /// Generic server error (-32000).
    pub fn server(msg: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: msg.into(),
            data: None,
        }
    }
}

/// A JSON-RPC 2.0 request (id `None` ⇒ notification, no reply expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Always the string `"2.0"`.
    pub jsonrpc: String,
    /// `None` makes this a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Method name.
    pub method: String,
    /// Parameters (positional or named). Defaults to `null`.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Build a request (call) with a numeric id.
    pub fn call(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(Id::Num(id)),
            method: method.into(),
            params,
        }
    }
    /// Build a notification (no id, no reply).
    pub fn notify(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Always the string `"2.0"`.
    pub jsonrpc: String,
    /// Echoed id of the matching request.
    pub id: Id,
    /// Present on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// Successful response.
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    /// Error response.
    pub fn err(id: Id, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// Handler trait + Router
// ═══════════════════════════════════════════════════════════════

/// Dispatches a JSON-RPC [`Request`] to its method implementation.
#[async_trait]
pub trait RpcHandler: Send + Sync {
    /// Invoke the method named in `req`, returning either a result value or an
    /// [`RpcError`]. Notifications (no id) are still dispatched; the server
    /// simply discards the returned value for them.
    async fn handle(&self, req: &Request) -> Result<Value, RpcError>;
}

/// A boxed async handler closure: `Fn(params) -> BoxFuture<Result<Value>>`.
pub type HandlerFn =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;

/// A simple method-name → handler router that implements [`RpcHandler`].
pub struct Router {
    handlers: std::collections::HashMap<String, HandlerFn>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    /// Create an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: std::collections::HashMap::new(),
        }
    }

    /// Register a handler for `name`.
    ///
    /// `f` is a sync function returning a boxed future, e.g.
    /// `|params| Box::pin(async move { Ok(json!("pong")) })`.
    pub fn route<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync + 'static,
    {
        self.handlers.insert(name.into(), Arc::new(f));
        self
    }
}

#[async_trait]
impl RpcHandler for Router {
    async fn handle(&self, req: &Request) -> Result<Value, RpcError> {
        match self.handlers.get(&req.method) {
            Some(f) => f(req.params.clone()).await,
            None => Err(RpcError::method_not_found(&req.method)),
        }
    }
}

// ── standard lifecycle method registration ─────────────────────

use crate::{
    DrainController, DrainResponse, HealthStatus, ProbeSink, ReadyStatus, ShutdownKind, methods,
};

impl Router {
    /// Register the standard lifecycle RPC methods (`Lifecycle.Drain`,
    /// `Lifecycle.Reload`, `Lifecycle.Status`, `Lifecycle.Health`) wired to a
    /// shared [`DrainController`] and an optional [`ProbeSink`].
    ///
    /// `Lifecycle.Drain` begins a graceful drain; `Lifecycle.Status` /
    /// `Lifecycle.Health` reflect the probe (or a minimal default when `probe`
    /// is `None`). Chain further `.route(...)` calls after this.
    #[must_use]
    pub fn lifecycle(
        self,
        drain: DrainController,
        probe: Option<std::sync::Arc<dyn ProbeSink>>,
    ) -> Self {
        let drain_for_rpc = drain.clone();
        let router = self.route(methods::DRAIN, move |_params| {
            let d = drain_for_rpc.clone();
            Box::pin(async move {
                d.begin_drain(ShutdownKind::Graceful);
                Ok(serde_json::to_value(DrainResponse {
                    accepted: true,
                    draining: true,
                })
                .unwrap())
            })
        });
        let drain_for_reload = drain.clone();
        let router = router.route(methods::RELOAD, move |_params| {
            let d = drain_for_reload.clone();
            Box::pin(async move {
                d.begin_drain(ShutdownKind::Reload);
                Ok(serde_json::Value::Null)
            })
        });
        let probe_for_status = probe.clone();
        let drain_for_status = drain.clone();
        let router = router.route(methods::STATUS, move |_params| {
            let p = probe_for_status.clone();
            let d = drain_for_status.clone();
            Box::pin(async move {
                let status = match &p {
                    Some(p) => p.ready().await,
                    None => {
                        let draining = d.is_draining();
                        ReadyStatus {
                            ready: !draining,
                            draining,
                            dependencies: Vec::new(),
                            generation: None,
                        }
                    }
                };
                Ok(serde_json::to_value(status).unwrap())
            })
        });
        let probe_for_health = probe.clone();
        router.route(methods::HEALTH, move |_params| {
            let p = probe_for_health.clone();
            Box::pin(async move {
                let health = match &p {
                    Some(p) => p.health().await,
                    None => HealthStatus {
                        alive: true,
                        pid: std::process::id(),
                        uptime_secs: 0,
                        version: "unknown".to_string(),
                    },
                };
                Ok(serde_json::to_value(health).unwrap())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_ok_serializes() {
        let r = Response::ok(Id::Num(1), json!("pong"));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"result\""));
        assert!(!s.contains("error"));
    }

    #[test]
    fn request_roundtrip() {
        let r = Request::call(7, "Lifecycle.Status", json!({"verbose": true}));
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "Lifecycle.Status");
        assert_eq!(back.id, Some(Id::Num(7)));
    }

    #[test]
    fn notification_has_no_id() {
        let r = Request::notify("Lifecycle.Heartbeat", json!({}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"id\""));
    }
}
