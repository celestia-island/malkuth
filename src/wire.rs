//! Wire-level transport trait contracts.
//!
//! A [`WireConn`] is a framed connection: it reads/writes one JSON value per
//! message. The generic [`FramedConn<S>`] in [`crate::codec`] adapts any
//! `tokio::io` duplex stream into a `WireConn`.

use serde_json::Value;
use std::io;

use async_trait::async_trait;

/// A framed, object-safe JSON-RPC connection.
#[async_trait]
pub trait WireConn: Send {
    async fn read_msg(&mut self) -> io::Result<Option<Value>>;
    async fn write_msg(&mut self, msg: &Value) -> io::Result<()>;
}

/// A server-side listener that yields accepted [`WireConn`]s.
#[async_trait]
pub trait WireListener: Send + Sync {
    async fn accept(&self) -> io::Result<Box<dyn WireConn>>;
    fn local_addr(&self) -> io::Result<String>;
}

/// A connection factory + listener factory addressed by string.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>>;
    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>>;
    fn name(&self) -> &'static str;
}
