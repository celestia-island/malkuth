//! Runtime-neutral wire-transport contracts.
//!
//! These traits describe a framed JSON-RPC connection in terms of plain
//! `serde_json::Value` messages — they carry **no** async-I/O trait in their
//! signatures, so they bind to no specific runtime. The concrete framing
//! adapter (`FramedConn`) and the transport backends (tcp/ws/ipc) live in the
//! `malkuth` crate and are tokio-based.

use std::io;

use async_trait::async_trait;
use serde_json::Value;

/// A framed, object-safe JSON-RPC connection.
#[async_trait]
pub trait WireConn: Send {
    /// Read the next message, or `None` if the peer closed cleanly.
    async fn read_msg(&mut self) -> io::Result<Option<Value>>;
    /// Write one message (newline-delimited on the wire) and flush.
    async fn write_msg(&mut self, msg: &Value) -> io::Result<()>;
}

/// A server-side listener that yields accepted [`WireConn`]s.
#[async_trait]
pub trait WireListener: Send + Sync {
    /// Accept the next inbound framed connection.
    async fn accept(&self) -> io::Result<Box<dyn WireConn>>;
    /// The locally-bound address (e.g. `127.0.0.1:54321`), for port-0 binds.
    fn local_addr(&self) -> io::Result<String>;
}

/// A connection factory + listener factory addressed by string.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Start listening on `addr`.
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>>;
    /// Dial `addr` and return one framed connection.
    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>>;
    /// Human-readable name of this transport (e.g. `"tcp"`, `"ws"`, `"ipc"`).
    fn name(&self) -> &'static str;
}
