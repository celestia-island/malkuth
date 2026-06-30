//! End-to-end JSON-RPC over TCP, driven under DIFFERENT runtimes to prove the
//! library is genuinely runtime-agnostic (it only depends on futures_io).

use std::sync::Arc;
use malkuth::{Router, Server, Client};
use malkuth::transport::TcpTransport;
use malkuth_core::Transport;
use serde_json::json;

fn handler() -> Arc<Router> {
    Arc::new(Router::new().route("ping", |_p| Box::pin(async { Ok(json!("pong")) })))
}

#[tokio::test]
async fn tcp_ping_pong_under_tokio() {
    let t = TcpTransport;
    let lis = t.listen("tcp://127.0.0.1:0").await.unwrap();
    let addr = lis.local_addr().unwrap();
    let h = handler();
    tokio::spawn(async move { let _ = Server::serve(&t, &format!("tcp://{addr}"), h).await; });
    // brief wait for the server to (re)bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut c = Client::connect(&TcpTransport, &format!("tcp://{addr}")).await.unwrap();
    let r = c.call("ping", json!({})).await.unwrap();
    assert_eq!(r, json!("pong"));
}

#[async_std::test]
async fn tcp_ping_pong_under_async_std() {
    let t = TcpTransport;
    let lis = t.listen("tcp://127.0.0.1:0").await.unwrap();
    let addr = lis.local_addr().unwrap();
    let h = handler();
    async_std::task::spawn(async move { let _ = Server::serve(&t, &format!("tcp://{addr}"), h).await; });
    async_std::task::sleep(std::time::Duration::from_millis(50)).await;
    let mut c = Client::connect(&TcpTransport, &format!("tcp://{addr}")).await.unwrap();
    let r = c.call("ping", json!({})).await.unwrap();
    assert_eq!(r, json!("pong"));
}
