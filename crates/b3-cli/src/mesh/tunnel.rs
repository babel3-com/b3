//! Userspace WireGuard tunnel via boringtun + smoltcp.
//!
//! Architecture:
//!   TCP client (reqwest/SSE) → 127.0.0.1:13000 → smoltcp virtual TCP →
//!   IP packets → boringtun encrypt → UDP to relay:51820 →
//!   relay decrypts → 10.44.0.1:3100 (API server)
//!
//! No TUN device, no root required. The smoltcp crate provides a userspace
//! TCP/IP stack, and boringtun handles WireGuard encryption.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use boringtun::noise::{Tunn, TunnResult};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;

use super::config::WgConfig;
use crate::config::{mesh_proxy_port, server_mesh_ip, server_mesh_port};

/// Maximum WireGuard packet size (MTU 1420 + overhead).
const MAX_PACKET: usize = 2048;
/// smoltcp TCP receive buffer size per socket.
const TCP_RX_BUF: usize = 65536;
/// smoltcp TCP transmit buffer size per socket.
const TCP_TX_BUF: usize = 65536;

/// Handle to the running tunnel. Drop or call shutdown() to stop.
pub struct TunnelHandle {
    pub local_addr: SocketAddr,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TunnelHandle {
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        !self.join_handle.is_finished()
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        let _ = self.join_handle.await;
    }
}

/// Start the WireGuard tunnel. Returns a handle with the local proxy address.
pub async fn start_tunnel(wg_config: WgConfig) -> anyhow::Result<TunnelHandle> {
    // 1. Create boringtun tunnel
    let private_key = boringtun::x25519::StaticSecret::from(wg_config.private_key);
    let peer_public = boringtun::x25519::PublicKey::from(wg_config.peer_public_key);
    let tunn = Tunn::new(
        private_key,
        peer_public,
        None, // no preshared key
        Some(wg_config.keepalive),
        0,    // tunnel index
        None, // no rate limiter
    );

    // 2. Bind UDP socket for WireGuard traffic
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(wg_config.peer_endpoint).await?;

    // 3. Bind local TCP listener for proxy
    //    Try the configured port first; if already taken (another agent on
    //    the same machine), fall back to an OS-assigned free port.
    let proxy_port = mesh_proxy_port();
    let bind_addr = format!("127.0.0.1:{proxy_port}");
    let tcp_listener = match TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(_) => {
            tracing::warn!("Port {proxy_port} in use, binding to random free port");
            TcpListener::bind("127.0.0.1:0").await
                .map_err(|e| anyhow::anyhow!("Cannot bind tunnel proxy: {e}"))?
        }
    };
    let local_addr = tcp_listener.local_addr()?;

    // 4. Shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // 5. Spawn the tunnel event loop
    let source_ip = wg_config.source_ip;
    let join_handle = tokio::spawn(async move {
        if let Err(e) = tunnel_loop(tunn, udp, tcp_listener, source_ip, shutdown_rx).await {
            tracing::error!("Tunnel loop exited with error: {e}");
        }
    });

    Ok(TunnelHandle {
        local_addr,
        shutdown_tx: Some(shutdown_tx),
        join_handle,
    })
}

// ── Virtual Device for smoltcp ↔ boringtun bridge ─────────────────

/// Packets queued for delivery between smoltcp and boringtun.
struct VirtualDevice {
    /// IP packets received from WireGuard (decapsulated), ready for smoltcp ingress.
    rx_queue: VecDeque<Vec<u8>>,
    /// IP packets produced by smoltcp egress, ready for WireGuard encapsulation.
    tx_queue: VecDeque<Vec<u8>>,
}

impl VirtualDevice {
    fn new() -> Self {
        Self {
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
        }
    }
}

impl Device for VirtualDevice {
    type RxToken<'a> = VirtualRxToken;
    type TxToken<'a> = VirtualTxToken<'a>;

    fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(packet) = self.rx_queue.pop_front() {
            Some((
                VirtualRxToken(packet),
                VirtualTxToken(&mut self.tx_queue),
            ))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VirtualTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 1420; // WireGuard MTU
        caps
    }
}

struct VirtualRxToken(Vec<u8>);

impl RxToken for VirtualRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct VirtualTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl<'a> TxToken for VirtualTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        self.0.push_back(buf);
        result
    }
}

// ── Tunnel Event Loop ──────────────────────────────────────────────

/// State for one proxied TCP connection.
struct ProxiedConnection {
    /// Local TCP stream (from reqwest/SSE client).
    stream: TcpStream,
    /// Handle to the smoltcp TCP socket.
    socket_handle: SocketHandle,
    /// Data read from local stream, pending write to smoltcp socket.
    local_to_mesh: Vec<u8>,
    /// Whether the local read side is done (client closed or errored).
    local_read_done: bool,
}

/// Destination for mesh traffic: the API server on the relay.
fn server_mesh_addr() -> (IpAddress, u16) {
    let ip: Ipv4Addr = server_mesh_ip().parse().expect("Invalid B3_SERVER_MESH_IP");
    (smoltcp::wire::IpAddress::Ipv4(ip), server_mesh_port())
}

/// Main tunnel event loop.
async fn tunnel_loop(
    tunn: Tunn,
    udp: UdpSocket,
    tcp_listener: TcpListener,
    source_ip: Ipv4Addr,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let tunn = Arc::new(Mutex::new(tunn));

    // Set up smoltcp interface
    let mut device = VirtualDevice::new();
    let config = IfaceConfig::new(HardwareAddress::Ip);
    let now = SmolInstant::from_millis(epoch_millis());
    let mut iface = Interface::new(config, &mut device, now);
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(
            smoltcp::wire::IpAddress::Ipv4(source_ip),
            16, // /16 mesh network
        )).unwrap();
    });

    let mut sockets = SocketSet::new(vec![]);
    let mut connections: Vec<ProxiedConnection> = Vec::new();

    // Buffers for boringtun operations
    let mut udp_recv_buf = vec![0u8; MAX_PACKET];
    let mut wg_buf = vec![0u8; MAX_PACKET];

    // Timer interval for smoltcp poll and WG keepalive
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_millis(50));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Initiate WireGuard handshake immediately
    {
        let mut t = tunn.lock().await;
        let result = t.format_handshake_initiation(&mut wg_buf, false);
        if let TunnResult::WriteToNetwork(data) = result {
            let _ = udp.send(data).await;
        }
    }

    loop {
        tokio::select! {
            // Shutdown signal
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("Tunnel shutdown requested");
                    break;
                }
            }

            // New local TCP connection
            Ok((stream, _addr)) = tcp_listener.accept() => {
                let tcp_rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
                let tcp_tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_TX_BUF]);
                let mut socket = tcp::Socket::new(tcp_rx_buf, tcp_tx_buf);

                let (dest_ip, dest_port) = server_mesh_addr();
                // Use an ephemeral source port
                let src_port = 49152 + (connections.len() as u16 % 16384);
                if let Err(e) = socket.connect(
                    iface.context(),
                    (dest_ip, dest_port),
                    src_port,
                ) {
                    tracing::warn!("smoltcp connect failed: {e}");
                    continue;
                }

                let handle = sockets.add(socket);
                connections.push(ProxiedConnection {
                    stream,
                    socket_handle: handle,
                    local_to_mesh: Vec::new(),
                    local_read_done: false,
                });
            }

            // Incoming UDP from relay (encrypted WireGuard packet)
            Ok(n) = udp.recv(&mut udp_recv_buf) => {
                let mut t = tunn.lock().await;
                let result = t.decapsulate(None, &udp_recv_buf[..n], &mut wg_buf);
                match result {
                    TunnResult::Done => {}
                    TunnResult::Err(e) => {
                        tracing::trace!("WG decapsulate error: {e:?}");
                    }
                    TunnResult::WriteToNetwork(data) => {
                        // Handshake response — send, then drain queued packets
                        let _ = udp.send(data).await;
                        loop {
                            let next = t.decapsulate(None, &[], &mut wg_buf);
                            match next {
                                TunnResult::WriteToNetwork(d) => {
                                    let _ = udp.send(d).await;
                                }
                                TunnResult::WriteToTunnelV4(d, _) => {
                                    device.rx_queue.push_back(d.to_vec());
                                }
                                _ => break,
                            }
                        }
                    }
                    TunnResult::WriteToTunnelV4(data, _) => {
                        device.rx_queue.push_back(data.to_vec());
                    }
                    TunnResult::WriteToTunnelV6(_, _) => {}
                }
            }

            // Timer tick: drive smoltcp + WG timers + bridge data
            _ = poll_interval.tick() => {
                // WireGuard timer maintenance (keepalives, handshake retries)
                {
                    let mut t = tunn.lock().await;
                    let result = t.update_timers(&mut wg_buf);
                    if let TunnResult::WriteToNetwork(data) = result {
                        let _ = udp.send(data).await;
                    }
                }

                // Bridge data between local TCP streams and smoltcp sockets
                bridge_connections(&mut connections, &mut sockets).await;

                // Run smoltcp poll (processes ingress from device.rx_queue, generates egress to device.tx_queue)
                let now = SmolInstant::from_millis(epoch_millis());
                iface.poll(now, &mut device, &mut sockets);

                // Encapsulate outgoing IP packets from smoltcp through WireGuard
                while let Some(ip_packet) = device.tx_queue.pop_front() {
                    let mut t = tunn.lock().await;
                    let result = t.encapsulate(&ip_packet, &mut wg_buf);
                    if let TunnResult::WriteToNetwork(data) = result {
                        let _ = udp.send(data).await;
                    }
                }

                // Clean up closed connections
                connections.retain(|conn| {
                    let socket = sockets.get::<tcp::Socket>(conn.socket_handle);
                    // Keep if socket is open or has data pending
                    socket.is_open() || socket.may_recv() || socket.may_send()
                });
            }
        }
    }

    Ok(())
}

/// Bridge data between local TCP streams and their corresponding smoltcp sockets.
async fn bridge_connections(
    connections: &mut Vec<ProxiedConnection>,
    sockets: &mut SocketSet<'_>,
) {
    for conn in connections.iter_mut() {
        let socket = sockets.get_mut::<tcp::Socket>(conn.socket_handle);

        // Read from local stream → buffer for smoltcp
        if !conn.local_read_done {
            let mut buf = [0u8; 4096];
            match conn.stream.try_read(&mut buf) {
                Ok(0) => {
                    conn.local_read_done = true;
                    // Signal EOF to smoltcp
                    socket.close();
                }
                Ok(n) => {
                    conn.local_to_mesh.extend_from_slice(&buf[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available yet
                }
                Err(_e) => {
                    conn.local_read_done = true;
                    socket.close();
                }
            }
        }

        // Write buffered data to smoltcp socket
        if !conn.local_to_mesh.is_empty() && socket.can_send() {
            match socket.send_slice(&conn.local_to_mesh) {
                Ok(sent) => {
                    conn.local_to_mesh.drain(..sent);
                }
                Err(_) => {
                    conn.local_to_mesh.clear();
                }
            }
        }

        // Read from smoltcp socket → write to local stream
        if socket.can_recv() {
            let mut buf = [0u8; 4096];
            match socket.recv_slice(&mut buf) {
                Ok(n) if n > 0 => {
                    // try_write is non-blocking
                    let _ = conn.stream.try_write(&buf[..n]);
                }
                _ => {}
            }
        }
    }
}

/// Current time in milliseconds since epoch, for smoltcp timestamps.
fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
