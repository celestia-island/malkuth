//! WebSocket transport via [`async_tungstenite`] (no runtime feature — works
//! under tokio / async-std / smol). One JSON value per WS text message (WS
//! frames are already delimited, so no NDJSON framing is needed here).

use std::io;

use async_trait::async_trait;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::{WebSocketStream, accept_async, client_async};
use futures_util::{SinkExt, StreamExt};
use malkuth_core::{Transport, WireConn, WireListener};
use serde_json::Value;
use tracing::debug;

/// Strip a `ws://` / `wss://` scheme prefix.
fn strip_scheme(addr: &str) -> &str {
    addr.strip_prefix("ws://")
        .or_else(|| addr.strip_prefix("wss://"))
        .unwrap_or(addr)
}

/// Split a ws address into `(host:port, full_url)`.
fn split_ws(addr: &str) -> (String, String) {
    let url = if addr.contains("://") { addr.to_string() } else { format!("ws://{addr}") };
    let without = strip_scheme(&url);
    let (hp, _) = without.split_once('/').unwrap_or((without, ""));
    (hp.to_string(), url)
}

/// WebSocket transport (plain `ws://`).
pub struct WsTransport;

#[async_trait]
impl Transport for WsTransport {
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>> {
        let hp = strip_scheme(addr).split('/').next().unwrap_or("").to_string();
        let listener = async_net::TcpListener::bind(&hp).await?;
        Ok(Box::new(WsWireListener { listener }))
    }

    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>> {
        let (hp, url) = split_ws(addr);
        let stream = async_net::TcpStream::connect(&hp).await?;
        let (ws, _resp) = client_async(url, stream)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("ws connect: {e}")))?;
        Ok(Box::new(WsConn { ws }))
    }

    fn name(&self) -> &'static str {
        "ws"
    }
}

pub struct WsWireListener {
    listener: async_net::TcpListener,
}

#[async_trait]
impl WireListener for WsWireListener {
    async fn accept(&self) -> io::Result<Box<dyn WireConn>> {
        let (tcp, _peer) = self.listener.accept().await?;
        let ws = accept_async(tcp)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("ws accept: {e}")))?;
        Ok(Box::new(WsConn { ws }))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(format!("{}", self.listener.local_addr()?))
    }
}

/// A WebSocket connection framed as JSON messages.
pub struct WsConn {
    ws: WebSocketStream<async_net::TcpStream>,
}

#[async_trait]
impl WireConn for WsConn {
    async fn read_msg(&mut self) -> io::Result<Option<Value>> {
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) => {
                    debug!(error = %e, "ws read error");
                    return Err(io::Error::new(io::ErrorKind::Other, format!("ws read: {e}")));
                }
                Some(Ok(msg)) => {
                    if msg.is_close() {
                        return Ok(None);
                    }
                    if msg.is_ping() || msg.is_pong() {
                        continue;
                    }
                    if let Ok(txt) = msg.into_text() {
                        return Ok(Some(serde_json::from_str(&txt)?));
                    }
                    if let Ok(bytes) = msg.into_data() {
                        return Ok(Some(serde_json::from_slice(&bytes)?));
                    }
                }
            }
        }
    }

    async fn write_msg(&mut self, msg: &Value) -> io::Result<()> {
        let s = serde_json::to_string(msg)?;
        self.ws
            .send(Message::text(s))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("ws write: {e}")))?;
        Ok(())
    }
}
