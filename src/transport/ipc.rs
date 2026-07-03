//! Local IPC transport via [`interprocess`] (Unix domain sockets on Unix, named
//! pipes on Windows), tokio async. Address forms: `ipc:/full/path` (filesystem
//! socket path) or `ipc:name` (a short / namespaced name).

use std::io;

use crate::{Transport, WireConn, WireListener};
use async_trait::async_trait;
use interprocess::local_socket::tokio::{
    Listener as LocalSocketListener, Stream as LocalSocketStream,
};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, ToFsName, ToNsName,
};

use crate::codec::FramedConn;

fn name_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid local-socket name: {e}"),
    )
}

fn to_name(addr: &str) -> io::Result<Name<'_>> {
    let s = addr.strip_prefix("ipc:").unwrap_or(addr);
    if s.starts_with('/') {
        s.to_fs_name::<GenericFilePath>().map_err(name_err)
    } else {
        s.to_ns_name::<GenericNamespaced>().map_err(name_err)
    }
}

/// Local IPC transport.
pub struct IpcTransport;

#[async_trait]
impl Transport for IpcTransport {
    async fn listen(&self, addr: &str) -> io::Result<Box<dyn WireListener>> {
        let name = to_name(addr)?;
        #[cfg(unix)]
        if let Some(path) = addr.strip_prefix("ipc:").filter(|p| p.starts_with('/')) {
            let _ = std::fs::remove_file(path);
        }
        let listener = ListenerOptions::new().name(name).create_tokio()?;
        Ok(Box::new(IpcWireListener { listener }))
    }

    async fn connect(&self, addr: &str) -> io::Result<Box<dyn WireConn>> {
        let name = to_name(addr)?;
        let stream = LocalSocketStream::connect(name).await?;
        Ok(Box::new(FramedConn::new(stream)))
    }

    fn name(&self) -> &'static str {
        "ipc"
    }
}

pub struct IpcWireListener {
    listener: LocalSocketListener,
}

#[async_trait]
impl WireListener for IpcWireListener {
    async fn accept(&self) -> io::Result<Box<dyn WireConn>> {
        let stream = self.listener.accept().await?;
        Ok(Box::new(FramedConn::new(stream)))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok("ipc:local".to_string())
    }
}
