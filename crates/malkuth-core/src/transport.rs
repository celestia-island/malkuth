//! Runtime-agnostic wire-transport contracts.
//!
//! A [`WireConn`] is a **framed** connection: it reads/writes one JSON value per
//! message, newline-delimited (NDJSON). The generic [`FramedConn<S>`] adapts
//! any `AsyncRead + AsyncWrite` stream — `async_net::TcpStream`, an adapted
//! tokio stream, a WebSocket byte adapter — into a `WireConn` with no glue.
//!
//! Everything sits on the `futures_io` traits, so the codec and the
//! server/client in the `malkuth` crate run under tokio, async-std and smol
//! alike — only the top-level executor differs.

use std::io;

use async_trait::async_trait;
use futures_io::{AsyncRead, AsyncWrite};
use futures_util::io::{AsyncReadExt, AsyncWriteExt};
use serde_json::Value;

/// A framed, object-safe JSON-RPC connection.
#[async_trait]
pub trait WireConn: Send {
    /// Read the next message, or `None` if the peer closed cleanly.
    async fn read_msg(&mut self) -> io::Result<Option<Value>>;
    /// Write one message (newline-delimited) and flush.
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

/// Try to pull one complete NDJSON frame out of `rd_buf`. Returns:
/// - `Some(Ok(v))` — a full frame was available and parsed.
/// - `Some(Err(_))` — a full frame was available but failed to parse.
/// - `None` — no newline yet; caller must read more bytes.
pub fn take_frame(rd_buf: &mut Vec<u8>) -> Option<io::Result<Value>> {
    let pos = rd_buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = rd_buf.drain(..=pos).collect();
    if line.iter().all(|b| b.is_ascii_whitespace()) {
        // A bare newline is a valid (empty) frame separator — treat as no-op.
        return Some(Ok(Value::Null));
    }
    Some(serde_json::from_slice(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)))
}

/// Generic NDJSON framing over any duplex stream.
pub struct FramedConn<S> {
    stream: S,
    rd_buf: Vec<u8>,
}

impl<S> FramedConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Wrap a duplex stream.
    pub fn new(stream: S) -> Self {
        Self { stream, rd_buf: Vec::with_capacity(8192) }
    }
}

#[async_trait]
impl<S> WireConn for FramedConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn read_msg(&mut self) -> io::Result<Option<Value>> {
        loop {
            if let Some(res) = take_frame(&mut self.rd_buf) {
                let v = res?;
                if v == Value::Null {
                    // bare newline: skip and continue
                    continue;
                }
                return Ok(Some(v));
            }
            let mut tmp = [0u8; 8192];
            let n = self.stream.read(&mut tmp).await?;
            if n == 0 {
                return if self.rd_buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed mid-frame",
                    ))
                };
            }
            self.rd_buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn write_msg(&mut self, msg: &Value) -> io::Result<()> {
        let mut data = serde_json::to_vec(msg)?;
        data.push(b'\n');
        self.stream.write_all(&data).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_frame_parses_complete_line() {
        let mut buf = b"{\"id\":1}\n".to_vec();
        let v = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(v["id"], 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn take_frame_none_when_incomplete() {
        let mut buf = b"{\"id\":1}".to_vec(); // no newline
        assert!(take_frame(&mut buf).is_none());
        assert_eq!(buf, b"{\"id\":1}"); // untouched
    }

    #[test]
    fn take_frame_keeps_leftover_after_frame() {
        let mut buf = b"a\nb\n".to_vec();
        let _ = take_frame(&mut buf).unwrap();
        assert_eq!(buf, b"b\n");
        let _ = take_frame(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn take_frame_invalid_json_is_error() {
        let mut buf = b"{bad\n".to_vec();
        assert!(take_frame(&mut buf).unwrap().is_err());
    }
}
