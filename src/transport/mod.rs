//! Pluggable transport backends for JSON-RPC (all tokio-based).

#[cfg(feature = "tcp")]
pub mod tcp;
#[cfg(feature = "tcp")]
pub use tcp::TcpTransport;

#[cfg(feature = "ws")]
pub mod ws;
#[cfg(feature = "ws")]
pub use ws::WsTransport;

#[cfg(feature = "ipc")]
pub mod ipc;
#[cfg(feature = "ipc")]
pub use ipc::IpcTransport;

/// A transport that dispatches by URL scheme to the built-in backends.
///
/// - `tcp://host:port` (or bare `host:port`) → [`TcpTransport`]
/// - `ws://…` / `wss://…` → [`WsTransport`]  (feature `ws`)
/// - `ipc:/path` or `ipc:name` → [`IpcTransport`]  (feature `ipc`)
///
/// For unrecognised schemes, falls back to TCP.
pub struct MultiTransport;

#[async_trait::async_trait]
impl crate::Transport for MultiTransport {
    async fn listen(&self, addr: &str) -> std::io::Result<Box<dyn crate::WireListener>> {
        self.pick(addr).listen(addr).await
    }
    async fn connect(&self, addr: &str) -> std::io::Result<Box<dyn crate::WireConn>> {
        self.pick(addr).connect(addr).await
    }
    fn name(&self) -> &'static str {
        "multi"
    }
}

impl MultiTransport {
    fn pick(&self, _addr: &str) -> Box<dyn crate::Transport> {
        #[cfg(feature = "ws")]
        if _addr.starts_with("ws://") || _addr.starts_with("wss://") {
            return Box::new(WsTransport);
        }
        #[cfg(feature = "ipc")]
        if _addr.starts_with("ipc:") {
            return Box::new(IpcTransport);
        }
        #[cfg(feature = "tcp")]
        {
            Box::new(TcpTransport)
        }
        #[cfg(not(feature = "tcp"))]
        {
            panic!("no transport feature enabled (enable tcp/ws/ipc)");
        }
    }
}
