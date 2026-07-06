//! The `Supervised` orchestrator + `Router::lifecycle`: a single `Lifecycle.Drain`
//! RPC call flips the shared drain bit the whole service observes.

use serde_json::json;
use std::sync::Arc;

use malkuth::{Client, Router, Supervised, Transport, transport::TcpTransport};

#[tokio::test]
async fn lifecycle_drain_rpc_flips_drain_bit() {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await.unwrap();
    let addr = lis.local_addr().unwrap();
    let dial = format!("tcp://{addr}");

    let supervised = Supervised::new();
    let ctrl = supervised.drain_controller();
    let ctrl_observed = ctrl.clone();

    let router = Arc::new(
        Router::new()
            .lifecycle(ctrl, None)
            .route("ping", |_| Box::pin(async { Ok(json!("pong")) })),
    );

    tokio::spawn(async move {
        let _ = supervised.serve_rpc_listener(lis, router).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let mut c = Client::connect(&TcpTransport, &dial).await.unwrap();
    // custom method still works alongside the lifecycle methods.
    assert_eq!(c.call("ping", json!({})).await.unwrap(), json!("pong"));
    assert!(!ctrl_observed.is_draining());

    // Lifecycle.Drain → shared drain bit flips on.
    let resp = c.call("Lifecycle.Drain", json!({})).await.unwrap();
    assert_eq!(resp["accepted"], json!(true));
    assert!(ctrl_observed.is_draining());
}
