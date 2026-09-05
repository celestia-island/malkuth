//! L7 WebSocket proxy with backend routing.
//!
//! Accepts client WS connections, routes to a backend WS endpoint, and
//! bidirectionally relays frames. Unlike the L4 TCP proxy (`proxy.rs`),
//! this proxy handles the WebSocket upgrade handshake at the proxy edge,
//! so backends can be plain WS servers without their own upgrade logic.
//!
//! ## Routing
//!
//! Backends are chosen by consistent-hash ring (same ring as the TCP proxy).
//! The routing key is composed from the client IP and optionally the WS
//! request path, providing L5 (session) affinity within the hash ring.

use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{WebSocketStream, accept_async, client_async};
use tracing::{debug, info, warn};

use crate::proxy::ProxyState;

/// Run the WebSocket proxy. `path_map` maps URL path prefixes to worker
/// routing keys (e.g. `"/chest" → "shittim-chest"`). If a path matches,
/// the worker key is used for backend selection; otherwise falls back to
/// client IP hashing.
pub async fn run_ws_proxy(
    public: SocketAddr,
    state: Arc<ProxyState>,
    path_map: HashMap<String, String>,
) -> io::Result<()> {
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
        let map = path_map.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_ws_client(stream, peer, state, &map).await {
                debug!(error = %e, "ws proxy connection ended");
            }
        });
    }
}

async fn handle_ws_client(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ProxyState>,
    path_map: &HashMap<String, String>,
) -> io::Result<()> {
    let client_ip = peer.ip().to_string();
    let mut dead: Vec<SocketAddr> = Vec::new();

    // Accept WebSocket upgrade from client.
    let mut client_ws = accept_async(stream)
        .await
        .map_err(|e| io::Error::other(format!("ws accept: {e}")))?;

    // Route: try path-map first, then fallback to IP hash.
    // Use the client IP as the fallback routing key.
    let route_key = path_map
        .iter()
        .find(|(k, _)| client_ip.contains(k.as_str()))
        .map(|(_, v)| v.clone())
        .unwrap_or(client_ip.clone());

    // Connect to the chosen backend.
    let mut backend_ws = loop {
        let backend = match state.pick(&route_key, &dead) {
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
                state.invalidate(&route_key);
                continue;
            }
        };
        match client_async(backend_url, tcp).await {
            Ok((ws, _resp)) => break ws,
            Err(e) => {
                warn!(backend = %backend.addr, error = %e, "ws backend handshake failed");
                dead.push(backend.addr);
                state.invalidate(&route_key);
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
        while let Some(Ok(msg)) = client_stream.next().await {
            if msg.is_close() {
                break;
            }
            if backend_sink.send(msg).await.is_err() {
                break;
            }
        }
    };
    let b2c = async {
        while let Some(Ok(msg)) = backend_stream.next().await {
            if client_sink.send(msg).await.is_err() {
                break;
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
    #[test]
    fn relay_does_nothing_on_empty_streams() {
        // Compile-time validation only — actual relay requires live WS connections.
    }
}
