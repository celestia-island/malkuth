//! JSON-RPC server (tokio). One task per connection via `tokio::spawn`.

use std::sync::Arc;

use crate::{Transport, WireConn, WireListener};
use tracing::{debug, warn};

use crate::jsonrpc::{Id, Request, Response, RpcHandler};

/// A JSON-RPC server.
pub struct Server;

impl Server {
    /// Bind `addr` on `transport`, then serve forever.
    pub async fn serve<H>(
        transport: &dyn Transport,
        addr: &str,
        handler: Arc<H>,
    ) -> std::io::Result<()>
    where
        H: RpcHandler + ?Sized + 'static,
    {
        let listener = transport.listen(addr).await?;
        Self::serve_listener(listener, handler).await
    }

    /// Serve on a pre-bound listener (avoids a double bind when you need
    /// [`WireListener::local_addr`] first).
    pub async fn serve_listener<H>(
        listener: Box<dyn WireListener>,
        handler: Arc<H>,
    ) -> std::io::Result<()>
    where
        H: RpcHandler + ?Sized + 'static,
    {
        loop {
            match listener.accept().await {
                Ok(conn) => {
                    let h = handler.clone();
                    tokio::spawn(async move {
                        serve_conn(conn, h).await;
                    });
                }
                Err(e) => warn!(error = %e, "accept failed"),
            }
        }
    }
}

async fn serve_conn<H>(mut conn: Box<dyn WireConn>, handler: Arc<H>)
where
    H: RpcHandler + ?Sized,
{
    loop {
        match conn.read_msg().await {
            Ok(Some(value)) => {
                let req: Request = match serde_json::from_value(value) {
                    Ok(r) => r,
                    Err(e) => {
                        debug!(error = %e, "malformed request frame");
                        continue;
                    }
                };
                let is_call = req.id.is_some();
                let id = req.id.clone().unwrap_or(Id::Null);
                let outcome = handler.handle(&req).await;
                if is_call {
                    let resp = match outcome {
                        Ok(v) => Response::ok(id, v),
                        Err(e) => Response::err(id, e),
                    };
                    let resp_val = serde_json::to_value(&resp).unwrap_or_else(|_| {
                        serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"encode failed"}})
                    });
                    if let Err(e) = conn.write_msg(&resp_val).await {
                        debug!(error = %e, "write failed; closing conn");
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                debug!(error = %e, "conn read error; closing");
                break;
            }
        }
    }
}
