//! Local IPC (Unix domain socket) proxy with sticky backend routing.
//!
//! Similar to the TCP proxy (`proxy.rs`) but uses Unix domain sockets on the
//! client-facing side. Backends are still addressed by TCP `SocketAddr`.
//! This allows local tooling to connect over Unix sockets while the proxy
//! routes to the appropriate backend process.
//!
//! Address form: `ipc:/path/to/socket` — the proxy listens at this path
//! and forwards to TCP backends via the consistent-hash ring.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};
use tokio::{
    io,
    net::TcpStream,
};
use interprocess::local_socket::{
    tokio::{Listener as LocalSocketListener, Stream as LocalSocketStream},
    traits::tokio::Listener as _,
    {GenericFilePath, ListenerOptions, Name, ToFsName},
};
use tracing::{debug, info, warn};

use crate::proxy::ProxyState;

/// Run the IPC proxy, listening on `socket_path` and forwarding to TCP backends.
pub async fn run_ipc_proxy(
    socket_path: &str,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    let path = socket_path.strip_prefix("ipc:").unwrap_or(socket_path);

    // Remove any stale socket file.
    let _ = std::fs::remove_file(path);

    let name = path.to_fs_name::<GenericFilePath>()
        .map_err(|e| io::Error::other(format!("invalid ipc name: {e}")))?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(io::Error::other)?;

    info!(event = "ipc_proxy_listening", path, "IPC proxy accepting");
    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "ipc proxy accept failed");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_ipc_client(stream, state).await {
                debug!(error = %e, "ipc proxy connection ended");
            }
        });
    }
}

async fn handle_ipc_client(
    mut client: LocalSocketStream,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    // IPC clients are local — use the process ID as a stable routing key.
    let client_key = format!("ipc-{}", std::process::id());
    let mut dead: Vec<SocketAddr> = Vec::new();

    let mut backend = loop {
        let b = match state.pick(&client_key, &dead) {
            Some(b) => b,
            None => {
                debug!("no healthy backend for ipc client; closing");
                return Ok(());
            }
        };
        match TcpStream::connect(b.addr).await {
            Ok(s) => break s,
            Err(e) => {
                warn!(backend = %b.addr, error = %e, "ipc→tcp backend connect failed");
                dead.push(b.addr);
                state.invalidate(&client_key);
            }
        }
    };

    info!(backend = ?dead.last().map(|a| a.to_string()).unwrap_or_default(), "ipc client proxied");
    let _ = io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}
