//! JSON-RPC client.
//!
//! [`Client`] holds one framed connection for sequential request/response calls.
//! For concurrent multi-call throughput, use [`ClientPool`] which manages N
//! long-lived connections and dispatches round-robin.

use serde_json::Value;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::oneshot;

use tracing::debug;

use crate::{
    Transport, WireConn,
    jsonrpc::{Id, Request, Response, RpcError},
};

// ═══════════════════════════════════════════════════════════════
// Single-connection sequential client
// ═══════════════════════════════════════════════════════════════

/// A simple request/response client over one long-lived connection.
///
/// `call()` sends a request and awaits the matching response. The connection
/// stays open across calls — no per-call TCP handshake. For concurrent
/// throughput, create a [`ClientPool`] with multiple connections.
pub struct Client {
    conn: Box<dyn WireConn>,
    next_id: AtomicU64,
}

impl Client {
    /// Connect to `addr` over `transport` (opens one TCP/WS/IPC connection).
    pub async fn connect(transport: &dyn Transport, addr: &str) -> std::io::Result<Self> {
        let conn = transport.connect(addr).await?;
        Ok(Self::from_conn(conn))
    }

    /// Wrap an already-established framed connection.
    pub fn from_conn(conn: Box<dyn WireConn>) -> Self {
        Self {
            conn,
            next_id: AtomicU64::new(1),
        }
    }

    /// Issue a call and await its result. The connection is held for the
    /// duration of the call — use a pool for concurrency.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::call(id, method, params);
        let req_val = serde_json::to_value(&req)
            .map_err(|e| RpcError::server(format!("encode request: {e}")))?;
        self.conn
            .write_msg(&req_val)
            .await
            .map_err(|e| RpcError::server(format!("write: {e}")))?;

        loop {
            let msg = self
                .conn
                .read_msg()
                .await
                .map_err(|e| RpcError::server(format!("read: {e}")))?
                .ok_or_else(|| RpcError::server("connection closed before response"))?;
            let resp: Response = serde_json::from_value(msg)
                .map_err(|e| RpcError::server(format!("decode response: {e}")))?;
            if resp.id != Id::Num(id) {
                debug!(?resp.id, "ignoring mismatched response id");
                continue;
            }
            if let Some(err) = resp.error {
                return Err(err);
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    /// Send a notification (no id, no reply expected).
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), RpcError> {
        let req = Request::notify(method, params);
        let req_val = serde_json::to_value(&req)
            .map_err(|e| RpcError::server(format!("encode notification: {e}")))?;
        self.conn
            .write_msg(&req_val)
            .await
            .map_err(|e| RpcError::server(format!("write: {e}")))?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════
// Multi-connection pool for concurrent throughput
// ═══════════════════════════════════════════════════════════════

/// Internal message: a call request sent from the pool to a connection task.
struct CallMsg {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value, RpcError>>,
}

/// A pool of N long-lived connections. Each connection is owned by a dedicated
/// tokio task that multiplexes reads and writes via `tokio::select!`, allowing
/// pipelined requests without head-of-line blocking.
///
/// Calls are dispatched round-robin across the pool.
pub struct ClientPool {
    senders: Vec<tokio::sync::mpsc::Sender<CallMsg>>,
    next: AtomicU64,
}

impl ClientPool {
    /// Create a pool of `size` connections to `addr` over `transport`.
    pub async fn new(transport: &dyn Transport, addr: &str, size: usize) -> std::io::Result<Self> {
        let mut senders = Vec::with_capacity(size);
        let mut next_id = 1u64;

        for _ in 0..size {
            let conn = transport.connect(addr).await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<CallMsg>(128);
            senders.push(tx);
            tokio::spawn(conn_task(conn, rx, next_id));
            next_id += 1_000_000; // give each conn a non-overlapping id range
        }

        Ok(Self {
            senders,
            next: AtomicU64::new(0),
        })
    }

    /// Issue a call through the pool. Dispatches to the next connection
    /// (round-robin). Multiple tasks can call this concurrently.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let len = self.senders.len();
        let idx = self.next.fetch_add(1, Ordering::Relaxed) as usize % len;

        // Generate an id unique within this connection's range. The per-conn
        // task increments from its base, so ids never collide across conns.
        let (reply_tx, reply_rx) = oneshot::channel();

        self.senders[idx]
            .send(CallMsg {
                method: method.to_string(),
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| RpcError::server("connection task closed"))?;

        reply_rx
            .await
            .map_err(|_| RpcError::server("connection task dropped reply"))?
    }
}

/// Per-connection task: owns one `WireConn` exclusively, multiplexes
/// read/write via `select!`. This is the key to long-connection performance —
/// no lock contention, no per-call connection setup.
async fn conn_task(
    mut conn: Box<dyn WireConn>,
    mut rx: tokio::sync::mpsc::Receiver<CallMsg>,
    id_base: u64,
) {
    let mut next_id = id_base;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, RpcError>>> = HashMap::new();

    loop {
        tokio::select! {
            // New call from the pool — write request to the wire.
            maybe_msg = rx.recv() => {
                let Some(msg) = maybe_msg else { return; };
                let id = next_id;
                next_id += 1;

                let req = Request::call(id, &msg.method, msg.params);
                let req_val = match serde_json::to_value(&req) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = msg.reply.send(Err(RpcError::server(format!("encode: {e}"))));
                        continue;
                    }
                };
                if let Err(e) = conn.write_msg(&req_val).await {
                    let _ = msg.reply.send(Err(RpcError::server(format!("write: {e}"))));
                    return; // connection dead
                }
                pending.insert(id, msg.reply);
            }
            // Response arrived from the wire — match to a pending caller.
            result = conn.read_msg() => {
                match result {
                    Ok(Some(value)) => {
                        let resp: Response = match serde_json::from_value(value) {
                            Ok(r) => r,
                            Err(e) => {
                                debug!(error = %e, "malformed response frame");
                                continue;
                            }
                        };
                        if let Id::Num(id) = resp.id {
                            if let Some(tx) = pending.remove(&id) {
                                let r = if let Some(err) = resp.error {
                                    Err(err)
                                } else {
                                    Ok(resp.result.unwrap_or(Value::Null))
                                };
                                let _ = tx.send(r);
                            }
                        }
                    }
                    Ok(None) => {
                        debug!("connection closed by peer");
                        return;
                    }
                    Err(e) => {
                        debug!(error = %e, "read error; closing connection");
                        return;
                    }
                }
            }
        }
    }
}
