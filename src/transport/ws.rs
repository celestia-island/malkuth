//! WebSocket transport via [`tokio_tungstenite`]. One JSON value per WS text
//! message (WS frames are already delimited — no NDJSON framing needed here).

use std::io;

use crate::{Transport, WireConn, WireListener};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async, client_async};
use tracing::debug;

fn strip_scheme(addr: &str) -> &str {
    addr.strip_prefix("ws://")
        .or_else(|| addr.strip_prefix("wss://"))
        .unwrap_or(addr)
}

fn split_ws(addr: &str) -> (String, String) {
    let url = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("ws://{addr}")
    };
    let without = strip_scheme(&url);
    let (hp, _) = without.split_once('/').unwrap_or((without, ""));
    (hp.to_string(), url)
}

/// WebSocket transport (plain `ws://`).
pub struct WsTransport;

#[async_trait]
impl Transport for WsTransport {
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>> {
        let hp = strip_scheme(addr)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        let listener = TcpListener::bind(&hp).await?;
        Ok(Box::new(WsWireListener { listener }))
    }

    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>> {
        let (hp, url) = split_ws(addr);
        let stream = TcpStream::connect(&hp).await?;
        let (ws, _resp) = client_async(url, stream)
            .await
            .map_err(|e| io::Error::other(format!("ws connect: {e}")))?;
        Ok(Box::new(WsConn { ws }))
    }

    fn name(&self) -> &'static str {
        "ws"
    }
}

pub struct WsWireListener {
    listener: TcpListener,
}

#[async_trait]
impl WireListener for WsWireListener {
    async fn accept(&self) -> io::Result<Box<dyn WireConn>> {
        let (tcp, _peer) = self.listener.accept().await?;
        let ws = accept_async(tcp)
            .await
            .map_err(|e| io::Error::other(format!("ws accept: {e}")))?;
        Ok(Box::new(WsConn { ws }))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(format!("{}", self.listener.local_addr()?))
    }
}

/// A WebSocket connection framed as JSON messages.
pub struct WsConn {
    ws: WebSocketStream<TcpStream>,
}

#[async_trait]
impl WireConn for WsConn {
    async fn read_msg(&mut self) -> io::Result<Option<Value>> {
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Err(e)) => {
                    debug!(error = %e, "ws read error");
                    return Err(io::Error::other(format!("ws read: {e}")));
                }
                Some(Ok(msg)) => {
                    if msg.is_close() {
                        return Ok(None);
                    }
                    if msg.is_ping() || msg.is_pong() {
                        continue;
                    }
                    if msg.is_text() {
                        let txt = msg.into_text().map_err(|e| {
                            io::Error::new(io::ErrorKind::InvalidData, format!("ws text: {e}"))
                        })?;
                        return Ok(Some(serde_json::from_str(&txt)?));
                    }
                    if msg.is_binary() {
                        let bytes = msg.into_data();
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
            .map_err(|e| io::Error::other(format!("ws write: {e}")))?;
        Ok(())
    }
}
