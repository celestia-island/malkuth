//! NDJSON framing over a tokio duplex stream.
//!
//! [`FramedConn`] wraps any `tokio::io` duplex stream and implements
//! [`WireConn`] by buffering reads until a newline and serializing writes with
//! a trailing newline + flush. [`take_frame`] is the pure framing helper,
//! extracted so it can be unit-tested without an executor.

use std::io;

use crate::WireConn;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Try to pull one complete NDJSON frame out of `rd_buf`.
pub fn take_frame(rd_buf: &mut Vec<u8>) -> Option<io::Result<Value>> {
    let pos = rd_buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = rd_buf.drain(..=pos).collect();
    if line.iter().all(|b| b.is_ascii_whitespace()) {
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
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    /// Wrap a duplex stream.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            rd_buf: Vec::with_capacity(8192),
        }
    }
}

#[async_trait]
impl<S> WireConn for FramedConn<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    async fn read_msg(&mut self) -> io::Result<Option<Value>> {
        loop {
            if let Some(res) = take_frame(&mut self.rd_buf) {
                let v = res?;
                if v == Value::Null {
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
        let mut buf = b"{\"id\":1}".to_vec();
        assert!(take_frame(&mut buf).is_none());
    }

    #[test]
    fn take_frame_invalid_json_is_error() {
        let mut buf = b"{bad\n".to_vec();
        assert!(take_frame(&mut buf).unwrap().is_err());
    }
}
