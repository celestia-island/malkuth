//! Batteries-included service orchestrator (tokio).
//!
//! [`Supervised`] composes the pluggable pieces: a shared [`DrainController`],
//! an optional [`ExitSource`] (OS signals by default with the `signals`
//! feature), and [`DrainHook`]s run during shutdown. [`Supervised::serve_rpc`]
//! races the JSON-RPC server against the exit source, then runs the drain hooks.

use std::{sync::Arc, time::Duration};

use crate::{
    DrainController, DrainHook, ExitSource, Server, Transport, WireListener, jsonrpc::RpcHandler,
};

/// A composable supervised service: drain controller + exit source + drain hooks.
pub struct Supervised {
    drain: DrainController,
    exit: Option<Arc<dyn ExitSource>>,
    hooks: Vec<Arc<dyn DrainHook>>,
    drain_budget: Duration,
}

impl Default for Supervised {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervised {
    #[must_use]
    pub fn new() -> Self {
        Self {
            drain: DrainController::new(),
            exit: None,
            hooks: Vec::new(),
            drain_budget: Duration::from_secs(30),
        }
    }

    pub fn drain_controller(&self) -> DrainController {
        self.drain.clone()
    }

    pub fn exit<E>(mut self, source: E) -> Self
    where
        E: ExitSource + 'static,
    {
        self.exit = Some(Arc::new(source));
        self
    }

    #[cfg(feature = "signals")]
    pub fn signals(self) -> Self {
        self.exit(crate::signals::SignalExitSource)
    }

    pub fn on_drain<H>(mut self, hook: H) -> Self
    where
        H: DrainHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn drain_budget(mut self, budget: Duration) -> Self {
        self.drain_budget = budget;
        self
    }

    pub async fn serve_rpc_listener<H>(
        self,
        listener: Box<dyn WireListener>,
        handler: Arc<H>,
    ) -> std::io::Result<()>
    where
        H: RpcHandler + ?Sized + 'static,
    {
        let ctrl = self.drain.clone();
        let server_fut = Server::serve_listener(listener, handler);
        match self.exit {
            None => {
                let _ = server_fut.await;
            }
            Some(exit) => {
                tokio::select! {
                    _ = server_fut => {}
                    _ = exit.wait(ctrl) => {}
                }
            }
        }
        for hook in self.hooks {
            let _ = hook.drain(self.drain_budget).await;
        }
        Ok(())
    }

    pub async fn serve_rpc<H>(
        self,
        transport: &dyn Transport,
        addr: &str,
        handler: Arc<H>,
    ) -> std::io::Result<()>
    where
        H: RpcHandler + ?Sized + 'static,
    {
        let listener = transport.listen(addr).await?;
        self.serve_rpc_listener(listener, handler).await
    }
}
