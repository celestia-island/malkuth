//! Runtime-agnostic wire-transport contracts.
//!
//! A [`WireStream`] is any byte stream that is both [`futures_io::AsyncRead`]
//! and [`futures_io::AsyncWrite`]. The `futures_io` traits are the *universal*
//! async-I/O traits: `async-net`, `async-tungstenite` and `interprocess`
//! streams all implement them directly, and a tokio stream can be adapted via
//! `tokio_util::compat`. That single trait choice is what keeps the JSON-RPC
//! codec runtime-agnostic.
//!
//! A [`Transport`] produces listeners (`listen`) and outbound connections
//! (`connect`) addressed by a URL-ish string — concrete impls in the `malkuth`
//! crate cover `tcp://`, `unix://`/`ws://` and local-socket IPC.

use std::io;

use async_trait::async_trait;
use futures_io::{AsyncRead, AsyncWrite};

/// Any duplex byte stream usable as a JSON-RPC transport.
///
/// Blanket-implemented for every `AsyncRead + AsyncWrite + Send + Unpin` type,
/// so `async_net::TcpStream`, adapted tokio streams, WebSocket frames-to-bytes
/// adapters, etc. are all transports with no glue.
pub trait WireStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> WireStream for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// A server-side listener that yields [`WireStream`]s.
#[async_trait]
pub trait WireListener: Send + Sync {
    /// Accept the next inbound connection, or return an error.
    async fn accept(&self) -> io::Result<Box<dyn WireStream>>;
}

/// A connection factory + listener factory addressed by string.
///
/// Address schemes are interpreted by the concrete implementation in the
/// `malkuth` crate (e.g. `tcp://127.0.0.1:0`, `ws://host/path`,
/// `unix:/path/to/sock`). This trait is transport- and scheme-agnostic on
/// purpose: registering multiple schemes behind one `Transport` (or composing
/// several) is a deployment choice.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Start listening on `addr`; the returned listener yields accepted streams.
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>>;

    /// Dial `addr` and return one connected stream.
    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireStream>>;

    /// Human-readable name of this transport (e.g. `"tcp"`, `"ws"`, `"ipc"`).
    fn name(&self) -> &'static str;
}
