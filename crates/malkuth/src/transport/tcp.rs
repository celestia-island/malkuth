//! TCP transport via [`async_net`] — runtime-agnostic (works under tokio /
//! async-std / smol).

use async_trait::async_trait;
use malkuth_core::{FramedConn, Transport, WireConn, WireListener};

/// Strip an optional `tcp://` scheme prefix; pass the rest through.
fn strip(addr: &str) -> &str {
    addr.strip_prefix("tcp://").unwrap_or(addr)
}

/// TCP transport (IPv4/IPv6 loopback or remote).
pub struct TcpTransport;

#[async_trait]
impl Transport for TcpTransport {
    async fn listen(&self, addr: &str) -> std::io::Result<Box<dyn WireListener>> {
        let listener = async_net::TcpListener::bind(strip(addr)).await?;
        Ok(Box::new(TcpWireListener { listener }))
    }
    async fn connect(&self, addr: &str) -> std::io::Result<Box<dyn WireConn>> {
        let stream = async_net::TcpStream::connect(strip(addr)).await?;
        Ok(Box::new(FramedConn::new(stream)))
    }
    fn name(&self) -> &'static str {
        "tcp"
    }
}

pub struct TcpWireListener {
    listener: async_net::TcpListener,
}

#[async_trait]
impl WireListener for TcpWireListener {
    async fn accept(&self) -> std::io::Result<Box<dyn WireConn>> {
        let (stream, _peer) = self.listener.accept().await?;
        Ok(Box::new(FramedConn::new(stream)))
    }
    fn local_addr(&self) -> std::io::Result<String> {
        Ok(format!("{}", self.listener.local_addr()?))
    }
}
