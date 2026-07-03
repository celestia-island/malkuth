//! End-to-end JSON-RPC over TCP (tokio).

use malkuth::Transport;
use malkuth::transport::TcpTransport;
use malkuth::{Client, Router, Server};
use serde_json::json;
use std::sync::Arc;

fn handler() -> Arc<Router> {
    Arc::new(Router::new().route("ping", |_p| Box::pin(async { Ok(json!("pong")) })))
}

#[tokio::test]
async fn tcp_ping_pong() {
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await.unwrap();
    let dial = format!("tcp://{}", lis.local_addr().unwrap());
    let h = handler();
    tokio::spawn(async move {
        let _ = Server::serve_listener(lis, h).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let mut c = Client::connect(&TcpTransport, &dial).await.unwrap();
    assert_eq!(c.call("ping", json!({})).await.unwrap(), json!("pong"));
}

#[tokio::test]
async fn tcp_concurrent_clients() {
    // The spawn-per-connection server handles several clients at once.
    let lis = TcpTransport.listen("tcp://127.0.0.1:0").await.unwrap();
    let dial = format!("tcp://{}", lis.local_addr().unwrap());
    let h = handler();
    tokio::spawn(async move {
        let _ = Server::serve_listener(lis, h).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let dial = dial.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(&TcpTransport, &dial).await.unwrap();
            c.call("ping", json!({})).await.unwrap()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), json!("pong"));
    }
}
