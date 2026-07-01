//! Round-trip JSON-RPC over WebSocket and IPC, to verify every transport
//! backend works through the same runtime-agnostic server/client.

#![cfg(any(feature = "ws", feature = "ipc"))]

use malkuth::Transport;
use malkuth::{Client, Router, Server};
use serde_json::json;
use std::sync::Arc;

fn handler() -> Arc<Router> {
    Arc::new(Router::new().route("ping", |_p| Box::pin(async { Ok(json!("pong")) })))
}

#[cfg(feature = "ws")]
#[tokio::test]
async fn ws_roundtrip_under_tokio() {
    use malkuth::transport::WsTransport;
    let lis = WsTransport.listen("ws://127.0.0.1:0").await.unwrap();
    let addr = lis.local_addr().unwrap();
    let url = format!("ws://{addr}");
    let h = handler();
    tokio::spawn(async move {
        let _ = Server::serve_listener(lis, h).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let mut c = Client::connect(&WsTransport, &url).await.unwrap();
    assert_eq!(c.call("ping", json!({})).await.unwrap(), json!("pong"));
}

#[cfg(feature = "ipc")]
#[tokio::test]
async fn ipc_roundtrip_under_tokio() {
    use malkuth::transport::IpcTransport;
    let pid = std::process::id();
    let sock = format!("/tmp/malkuth_ipc_{pid}.sock");
    let _ = std::fs::remove_file(&sock);
    let addr = format!("ipc:{sock}");
    let lis = IpcTransport.listen(&addr).await.unwrap();
    let h = handler();
    tokio::spawn(async move {
        let _ = Server::serve_listener(lis, h).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let mut c = Client::connect(&IpcTransport, &addr).await.unwrap();
    assert_eq!(c.call("ping", json!({})).await.unwrap(), json!("pong"));
    let _ = std::fs::remove_file(&sock);
}
