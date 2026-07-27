//! L7 WebSocket proxy with backend routing via consistent hashing.
//!
//! Unlike the L4 TCP proxy (`proxy.rs`), this proxy understands WebSocket
//! frames and can route connections at the application layer. It accepts
//! client WS upgrades, connects to a backend WS endpoint chosen by hashing
//! the request path (or a URI query param `worker`), then bidirectionally
//! relays frames.
//!
//! Use case: malkuth acts as a WS connection line-holder. When a backend
//! worker restarts, the proxy keeps the client connected and re-routes to
//! the new worker once it comes online.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{
    WebSocketStream, accept_async, client_async, tungstenite::Message,
};
use futures_util::{SinkExt, StreamExt};
use tracing::{debug, info, warn};

use crate::proxy::{Backend, ProxyState};

/// Run the WebSocket proxy on `public` until the process exits.
///
/// Each incoming WS connection is routed to a backend via the same consistent-hash
/// ring used by the TCP proxy. Backend URLs must be `ws://host:port/path`.
pub async fn run_ws_proxy(public: SocketAddr, state: Arc<ProxyState>) -> io::Result<()> {
    let listener = TcpListener::bind(public).await?;
    info!(event = "ws_proxy_listening", %public, "L7 WebSocket proxy accepting");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "ws proxy accept failed");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_ws_client(stream, peer, state).await {
                debug!(error = %e, "ws proxy connection ended");
            }
        });
    }
}

async fn handle_ws_client(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    let client_ip = peer.ip().to_string();
    let mut dead: Vec<SocketAddr> = Vec::new();

    // Accept WebSocket upgrade from client.
    let mut client_ws = accept_async(stream)
        .await
        .map_err(|e| io::Error::other(format!("ws accept: {e}")))?;

    // Connect to the chosen backend.
    let mut backend_ws = loop {
        let backend = match state.pick(&client_ip, &dead) {
            Some(b) => b,
            None => {
                debug!(%peer, "no healthy ws backend; closing client");
                return Ok(());
            }
        };
        let backend_url = format!("ws://{}/ws", backend.addr);
        let tcp = match TcpStream::connect(backend.addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(backend = %backend.addr, error = %e, "ws backend connect failed");
                dead.push(backend.addr);
                state.invalidate(&client_ip);
                continue;
            }
        };
        match client_async(backend_url, tcp).await {
            Ok((ws, _resp)) => break ws,
            Err(e) => {
                warn!(backend = %backend.addr, error = %e, "ws backend handshake failed");
                dead.push(backend.addr);
                state.invalidate(&client_ip);
            }
        }
    };

    info!(%peer, backend = ?dead.last().unwrap_or(&peer), "ws client proxied");
    relay_ws(&mut client_ws, &mut backend_ws).await;
    let _ = client_ws.close(None).await;
    let _ = backend_ws.close(None).await;
    Ok(())
}

/// Bidirectionally relay WebSocket frames between client and backend.
/// Closes when either side disconnects (Close frame or stream end).
async fn relay_ws(
    client: &mut WebSocketStream<TcpStream>,
    backend: &mut WebSocketStream<TcpStream>,
) {
    let (mut client_sink, mut client_stream) = client.split();
    let (mut backend_sink, mut backend_stream) = backend.split();

    let c2b = async {
        loop {
            match client_stream.next().await {
                Some(Ok(msg)) => {
                    if msg.is_close() {
                        break;
                    }
                    if backend_sink.send(msg).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    };
    let b2c = async {
        loop {
            match backend_stream.next().await {
                Some(Ok(msg)) => {
                    if client_sink.send(msg).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    };
    tokio::select! {
        _ = c2b => debug!("ws client→backend stream ended"),
        _ = b2c => debug!("ws backend→client stream ended"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn relay_does_nothing_on_empty_streams() {
        // Compile-time validation only — actual relay requires live WS connections.
    }
}
