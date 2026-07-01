//! Minimal malkuth service: drain controller + JSON-RPC server + lifecycle methods.
//!
//! Run: `cargo run --example minimal_server --features tcp,signals`

use std::sync::Arc;

use malkuth::Transport;
use malkuth::transport::TcpTransport;
use malkuth::{Client, Router, Supervised};
use serde_json::json;

#[tokio::main]
async fn main() {
    // 1. Bind a TCP listener for JSON-RPC.
    let listener = TcpTransport
        .listen("tcp://127.0.0.1:0")
        .await
        .expect("bind failed");
    let addr = listener.local_addr().unwrap();
    println!("JSON-RPC listening on tcp://{addr}");

    // 2. Build the supervised service: signal handling + drain + custom routes.
    let supervised = Supervised::new().signals();
    let ctrl = supervised.drain_controller();

    let router = Arc::new(
        Router::new()
            .lifecycle(ctrl.clone(), None)
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    // 3. Serve until SIGTERM/SIGINT, then run drain hooks.
    println!("Press Ctrl-C to stop…");
    supervised
        .serve_rpc_listener(listener, router)
        .await
        .expect("serve failed");
    println!("Service stopped.");

    // --- demonstrate the client side ---
    let mut client = Client::connect(&TcpTransport, &format!("tcp://{addr}"))
        .await
        .expect("connect failed");
    let resp = client.call("ping", json!({})).await.unwrap();
    println!("ping → {resp}");
}
