//! TCP transport via [`tokio::net`] (loopback or remote).

use std::io;
use tokio::net::{TcpListener, TcpStream};

use async_trait::async_trait;

use crate::{Transport, WireConn, WireListener, codec::FramedConn};

fn strip(addr: &str) -> &str {
    addr.strip_prefix("tcp://").unwrap_or(addr)
}

/// TCP transport (IPv4/IPv6 loopback or remote).
pub struct TcpTransport;

#[async_trait]
impl Transport for TcpTransport {
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>> {
        let listener = TcpListener::bind(strip(addr)).await?;
        Ok(Box::new(TcpWireListener { listener }))
    }
    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>> {
        let stream = TcpStream::connect(strip(addr)).await?;
        Ok(Box::new(FramedConn::new(stream)))
    }
    fn name(&self) -> &'static str {
        "tcp"
    }
}

pub struct TcpWireListener {
    listener: TcpListener,
}

#[async_trait]
impl WireListener for TcpWireListener {
    async fn accept(&self) -> io::Result<Box<dyn WireConn>> {
        let (stream, _peer) = self.listener.accept().await?;
        Ok(Box::new(FramedConn::new(stream)))
    }
    fn local_addr(&self) -> io::Result<String> {
        Ok(format!("{}", self.listener.local_addr()?))
    }
}
