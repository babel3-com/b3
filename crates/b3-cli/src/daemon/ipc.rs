//! Cross-platform IPC for daemon ↔ client communication.
//!
//! - Unix: Unix domain sockets (~/.b3/b3.sock)
//! - Windows: TCP on localhost (127.0.0.1, random port stored in port file)
//!
//! Both transports provide AsyncRead + AsyncWrite streams that work
//! identically with the length-prefixed protocol in `protocol.rs`.

use std::path::PathBuf;

use crate::config::Config;

// ─── Unix implementation ────────────────────────────────────────────────────

#[cfg(unix)]
mod platform {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub struct IpcListener {
        inner: UnixListener,
    }

    pub struct IpcStream {
        inner: UnixStream,
    }

    impl IpcListener {
        pub fn bind() -> std::io::Result<Self> {
            let path = ipc_path();
            // Clean up stale socket
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;

            // Restrict permissions to owner only
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

            Ok(Self { inner: listener })
        }

        pub async fn accept(&self) -> std::io::Result<IpcStream> {
            let (stream, _addr) = self.inner.accept().await?;
            Ok(IpcStream { inner: stream })
        }
    }

    impl IpcStream {
        pub async fn connect() -> std::io::Result<Self> {
            let path = ipc_path();
            let stream = UnixStream::connect(&path).await?;
            Ok(Self { inner: stream })
        }

        pub fn into_split(self) -> (IpcReadHalf, IpcWriteHalf) {
            let (r, w) = self.inner.into_split();
            (IpcReadHalf::Unix(r), IpcWriteHalf::Unix(w))
        }
    }

    fn ipc_path() -> PathBuf {
        Config::config_dir().join("b3.sock")
    }

    /// Check if the IPC endpoint exists (socket file present).
    pub fn endpoint_exists() -> bool {
        ipc_path().exists()
    }

    /// Clean up the IPC endpoint.
    pub fn cleanup_endpoint() {
        let _ = std::fs::remove_file(ipc_path());
    }
}

#[cfg(unix)]
pub use platform::*;

// ─── Windows implementation ─────────────────────────────────────────────────

#[cfg(windows)]
mod platform {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    pub struct IpcListener {
        inner: TcpListener,
    }

    pub struct IpcStream {
        inner: TcpStream,
    }

    impl IpcListener {
        pub fn bind() -> std::io::Result<Self> {
            // Bind to random localhost port. Store the port for clients.
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = std_listener.local_addr()?.port();
            std_listener.set_nonblocking(true)?;

            // Write port to file so clients can find us
            let port_file = ipc_port_path();
            std::fs::write(&port_file, port.to_string())?;

            let listener = TcpListener::from_std(std_listener)?;
            Ok(Self { inner: listener })
        }

        pub async fn accept(&self) -> std::io::Result<IpcStream> {
            let (stream, _addr) = self.inner.accept().await?;
            Ok(IpcStream { inner: stream })
        }
    }

    impl IpcStream {
        pub async fn connect() -> std::io::Result<Self> {
            let port_file = ipc_port_path();
            let port_str = std::fs::read_to_string(&port_file).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Daemon port file not found: {e}"),
                )
            })?;
            let port: u16 = port_str.trim().parse().map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid port in daemon port file: {e}"),
                )
            })?;

            let stream = TcpStream::connect(format!("127.0.0.1:{port}")).await?;
            Ok(Self { inner: stream })
        }

        pub fn into_split(self) -> (IpcReadHalf, IpcWriteHalf) {
            let (r, w) = self.inner.into_split();
            (IpcReadHalf::Tcp(r), IpcWriteHalf::Tcp(w))
        }
    }

    fn ipc_port_path() -> PathBuf {
        Config::config_dir().join("b3-ipc.port")
    }

    /// Check if the IPC endpoint exists (port file present).
    pub fn endpoint_exists() -> bool {
        ipc_port_path().exists()
    }

    /// Clean up the IPC endpoint.
    pub fn cleanup_endpoint() {
        let _ = std::fs::remove_file(ipc_port_path());
    }
}

#[cfg(windows)]
pub use platform::*;

// ─── Split halves (platform-agnostic enum) ──────────────────────────────────

/// Read half of an IPC stream. Works with `protocol::read_message`.
pub enum IpcReadHalf {
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedReadHalf),
    #[cfg(windows)]
    Tcp(tokio::net::tcp::OwnedReadHalf),
}

/// Write half of an IPC stream. Works with `protocol::write_message`.
pub enum IpcWriteHalf {
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
    #[cfg(windows)]
    Tcp(tokio::net::tcp::OwnedWriteHalf),
}

// Implement AsyncRead/AsyncWrite via delegation

impl tokio::io::AsyncRead for IpcReadHalf {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            IpcReadHalf::Unix(r) => std::pin::Pin::new(r).poll_read(cx, buf),
            #[cfg(windows)]
            IpcReadHalf::Tcp(r) => std::pin::Pin::new(r).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for IpcWriteHalf {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            #[cfg(unix)]
            IpcWriteHalf::Unix(w) => std::pin::Pin::new(w).poll_write(cx, buf),
            #[cfg(windows)]
            IpcWriteHalf::Tcp(w) => std::pin::Pin::new(w).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            #[cfg(unix)]
            IpcWriteHalf::Unix(w) => std::pin::Pin::new(w).poll_flush(cx),
            #[cfg(windows)]
            IpcWriteHalf::Tcp(w) => std::pin::Pin::new(w).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            #[cfg(unix)]
            IpcWriteHalf::Unix(w) => std::pin::Pin::new(w).poll_shutdown(cx),
            #[cfg(windows)]
            IpcWriteHalf::Tcp(w) => std::pin::Pin::new(w).poll_shutdown(cx),
        }
    }
}
