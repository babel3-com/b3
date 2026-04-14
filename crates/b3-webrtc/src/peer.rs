//! B3Peer — WebRTC peer connection wrapper for Babel3.
//!
//! Bridges datachannel-rs callbacks to tokio channels, providing an async
//! interface for creating/accepting data channels and exchanging messages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use datachannel::{
    ConnectionState, DataChannelHandler, DataChannelInfo, GatheringState,
    IceCandidate, PeerConnectionHandler, RtcConfig, RtcDataChannel,
    RtcPeerConnection, SdpType, SessionDescription,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::channel::{
    ChannelMessage, ChannelReceiver, ChannelSender, ChunkAssembler, DecodeResult,
};
use crate::signaling::IceServer;

// ── Configuration ────────────────────────────────────────────────────

/// Configuration for creating a B3Peer.
pub struct PeerConfig {
    pub ice_servers: Vec<IceServer>,
    /// Force relay-only (TURN). Useful for testing TURN path.
    pub force_relay: bool,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            }],
            force_relay: false,
        }
    }
}

// ── Events emitted by the peer ───────────────────────────────────────

/// Events from the peer connection, delivered via tokio channel.
#[derive(Debug)]
pub enum PeerEvent {
    /// Local SDP generated — send to remote via signaling.
    LocalDescription { sdp: String, sdp_type: String },
    /// Local ICE candidate — send to remote via signaling.
    LocalCandidate { candidate: String, mid: String },
    /// ICE gathering complete.
    GatheringComplete,
    /// Connection state changed.
    ConnectionStateChange(String),
    /// A new data channel was opened by the remote peer.
    IncomingChannel {
        label: String,
        sender: ChannelSender,
        receiver: ChannelReceiver,
    },
}

// ── B3Peer ───────────────────────────────────────────────────────────

/// High-level WebRTC peer connection.
///
/// Wraps datachannel-rs with:
/// - Tokio channel bridge (callbacks → async)
/// - Automatic message framing and 16KB chunking
/// - Named channel management
pub struct B3Peer {
    pc: Box<RtcPeerConnection<B3PcHandler>>,
    /// Channels created locally (by us).
    local_channels: HashMap<String, Arc<Mutex<Option<Box<RtcDataChannel<B3DcHandler>>>>>>,
    /// Event receiver for the consumer.
    event_rx: mpsc::Receiver<PeerEvent>,
    /// Shared state for the callback handler.
    shared: Arc<Mutex<SharedState>>,
}

struct SharedState {
    event_tx: mpsc::Sender<PeerEvent>,
    /// Pending channel open notifications.
    pending_opens: HashMap<String, oneshot::Sender<()>>,
    /// Send-loop receivers for incoming channels (keyed by label).
    /// data_channel_handler stores them here; on_data_channel picks them up.
    incoming_send_rx: HashMap<String, mpsc::Receiver<Vec<u8>>>,
    /// Alive flags for incoming channels — send loop sets to false on DC error.
    incoming_alive_flags: HashMap<String, Arc<std::sync::atomic::AtomicBool>>,
    /// Keep incoming data channels alive (prevent drop while send loop is active).
    incoming_channels: Vec<Arc<Mutex<Option<Box<RtcDataChannel<B3DcHandler>>>>>>,
}

impl B3Peer {
    /// Create a new peer connection.
    pub fn new(config: PeerConfig) -> anyhow::Result<Self> {
        let ice_strings: Vec<String> = config
            .ice_servers
            .iter()
            .flat_map(|s| {
                s.urls.iter().map(move |url| {
                    if let (Some(user), Some(cred)) = (&s.username, &s.credential) {
                        // TURN format: turn:user:pass@host:port
                        // URL-encode colons in username (%3A) so libjuice doesn't
                        // split the user:pass delimiter at the wrong colon.
                        let encoded_user = user.replace(':', "%3A");
                        let encoded_cred = cred.replace(':', "%3A");
                        let (proto, rest) = url.split_once(':').unwrap_or(("stun", url));
                        format!("{proto}:{encoded_user}:{encoded_cred}@{rest}")
                    } else {
                        url.clone()
                    }
                })
            })
            .collect();

        let mut rtc_config = RtcConfig::new(&ice_strings);
        if config.force_relay {
            rtc_config.ice_transport_policy = datachannel::TransportPolicy::Relay;
        }

        let (event_tx, event_rx) = mpsc::channel(256);  // peer events

        let shared = Arc::new(Mutex::new(SharedState {
            event_tx,
            pending_opens: HashMap::new(),
            incoming_send_rx: HashMap::new(),
            incoming_alive_flags: HashMap::new(),
            incoming_channels: Vec::new(),
        }));

        let handler = B3PcHandler {
            shared: Arc::clone(&shared),
        };

        let pc = RtcPeerConnection::new(&rtc_config, handler)?;

        // Replace the datachannel crate's log callback with a panic-safe version.
        // RtcPeerConnection::new() triggers ensure_logging() (via Once) which installs
        // a callback that accesses tracing TLS. During tokio thread destruction, that
        // TLS is gone → panic → corrupted runtime. We overwrite it immediately.
        crate::init_safe_logging();

        Ok(Self {
            pc,
            local_channels: HashMap::new(),
            event_rx,
            shared,
        })
    }

    /// Create a local data channel (we are the offerer for this channel).
    ///
    /// Returns (sender, receiver, open_notify). The channel is usable
    /// after `open_notify` resolves.
    pub fn create_channel(
        &mut self,
        label: &str,
    ) -> anyhow::Result<(ChannelSender, ChannelReceiver, oneshot::Receiver<()>)> {
        let (msg_tx, msg_rx) = mpsc::channel::<Vec<u8>>(256);
        let (decoded_tx, decoded_rx) = mpsc::channel::<ChannelMessage>(256);
        let (open_tx, open_rx) = oneshot::channel();

        let dc_handler = B3DcHandler {
            label: label.to_string(),
            decoded_tx,
            assembler: None,
        };

        // Register open notification.
        {
            let mut state = self.shared.lock().unwrap();
            state.pending_opens.insert(label.to_string(), open_tx);
        }

        let dc = self.pc.create_data_channel(label, dc_handler)?;
        let dc_arc = Arc::new(Mutex::new(Some(dc)));

        // Alive flag — shared between send loop and ChannelSender.
        // Send loop sets to false on DC error; ChannelSender checks before sending.
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_for_loop = alive.clone();

        // Spawn send loop — bridges tokio channel to datachannel send.
        let dc_clone = Arc::clone(&dc_arc);
        tokio::spawn(async move {
            let mut rx = msg_rx;
            while let Some(frame) = rx.recv().await {
                let send_result = {
                    let mut guard = dc_clone.lock().unwrap();
                    if let Some(dc) = guard.as_mut() {
                        dc.send(&frame)
                    } else {
                        break;
                    }
                };
                if let Err(e) = send_result {
                    warn!("data channel send error: {e}");
                    alive_for_loop.store(false, std::sync::atomic::Ordering::Release);
                    break;
                }
            }
        });

        self.local_channels.insert(label.to_string(), dc_arc);

        let sender = ChannelSender::new(msg_tx, alive);
        let receiver = ChannelReceiver::new(decoded_rx);

        Ok((sender, receiver, open_rx))
    }

    /// Generate an SDP offer. The offer will be emitted as a `PeerEvent::LocalDescription`.
    ///
    /// NOTE: datachannel-rs / libdatachannel does not support ICE restart.
    /// On connection failure, the caller must drop this B3Peer entirely and
    /// create a new one. This is the only recovery path.
    pub fn create_offer(&mut self) -> anyhow::Result<()> {
        self.pc.set_local_description(SdpType::Offer)?;
        Ok(())
    }

    /// Apply a remote SDP description (offer or answer).
    pub fn set_remote_description(
        &mut self,
        sdp: &str,
        sdp_type: &str,
    ) -> anyhow::Result<()> {
        let ty = match sdp_type {
            "offer" => SdpType::Offer,
            "answer" => SdpType::Answer,
            "pranswer" => SdpType::Pranswer,
            "rollback" => SdpType::Rollback,
            other => anyhow::bail!("unknown SDP type: {other}"),
        };

        // SessionDescription is serde-serializable. Construct via JSON
        // to use the serde_sdp deserializer which calls parse_sdp().
        let sdp_type_str = match ty {
            SdpType::Offer => "offer",
            SdpType::Answer => "answer",
            SdpType::Pranswer => "pranswer",
            SdpType::Rollback => "rollback",
        };
        let json = serde_json::json!({
            "type": sdp_type_str,
            "sdp": sdp,
        });
        let sess_desc: SessionDescription = serde_json::from_value(json)
            .map_err(|e| anyhow::anyhow!("SDP parse error: {e}"))?;

        self.pc.set_remote_description(&sess_desc)?;
        Ok(())
    }

    /// Add a remote ICE candidate (trickle ICE).
    pub fn add_remote_candidate(&mut self, candidate: &str, mid: &str) -> anyhow::Result<()> {
        let cand = IceCandidate {
            candidate: candidate.to_string(),
            mid: mid.to_string(),
        };
        self.pc.add_remote_candidate(&cand)?;
        Ok(())
    }

    /// Receive the next peer event.
    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        self.event_rx.recv().await
    }

    /// Get the current local description (SDP with gathered ICE candidates).
    /// Call after ICE gathering completes to get the full SDP.
    pub fn local_description(&self) -> Option<String> {
        self.pc.local_description().map(|desc| desc.sdp.to_string())
    }
}

// ── datachannel-rs handler implementations ───────────────────────────

/// Peer connection handler — bridges callbacks to tokio channels.
struct B3PcHandler {
    shared: Arc<Mutex<SharedState>>,
}

impl PeerConnectionHandler for B3PcHandler {
    type DCH = B3DcHandler;

    fn data_channel_handler(&mut self, info: DataChannelInfo) -> Self::DCH {
        let (decoded_tx, decoded_rx) = mpsc::channel::<ChannelMessage>(64);
        let (msg_tx, msg_rx) = mpsc::channel::<Vec<u8>>(64);

        let label = info.label.clone();

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sender = ChannelSender::new(msg_tx, alive.clone());
        let receiver = ChannelReceiver::new(decoded_rx);

        // Emit incoming channel event with sender+receiver.
        let mut shared = self.shared.lock().unwrap();
        if let Err(e) = shared.event_tx.try_send(PeerEvent::IncomingChannel {
            label: label.clone(),
            sender,
            receiver,
        }) {
            warn!("incoming channel event dropped — channel full: {e}");
        }
        // Store send_rx and alive flag for on_data_channel to pick up
        shared.incoming_send_rx.insert(label.clone(), msg_rx);
        shared.incoming_alive_flags.insert(label.clone(), alive);

        B3DcHandler {
            label,
            decoded_tx,
            assembler: None,
        }
    }

    fn on_description(&mut self, sess_desc: SessionDescription) {
        let sdp_type = match sess_desc.sdp_type {
            SdpType::Offer => "offer",
            SdpType::Answer => "answer",
            SdpType::Pranswer => "pranswer",
            SdpType::Rollback => "rollback",
        };
        let sdp = sess_desc.sdp.to_string();
        debug!("local description generated: {sdp_type}");

        let shared = self.shared.lock().unwrap();
        if let Err(e) = shared.event_tx.try_send(PeerEvent::LocalDescription {
            sdp,
            sdp_type: sdp_type.to_string(),
        }) {
            warn!("local description event dropped — channel full: {e}");
        }
    }

    fn on_candidate(&mut self, cand: IceCandidate) {
        let shared = self.shared.lock().unwrap();
        if let Err(e) = shared.event_tx.try_send(PeerEvent::LocalCandidate {
            candidate: cand.candidate,
            mid: cand.mid,
        }) {
            warn!("ICE candidate dropped — event channel full: {e}");
        }
    }

    fn on_connection_state_change(&mut self, state: ConnectionState) {
        let state_str = format!("{state:?}").to_lowercase();
        info!("WebRTC connection state: {state_str}");

        let shared = self.shared.lock().unwrap();
        if let Err(e) = shared.event_tx.try_send(PeerEvent::ConnectionStateChange(state_str)) {
            warn!("connection state event dropped — channel full: {e}");
        }
    }

    fn on_gathering_state_change(&mut self, state: GatheringState) {
        debug!("ICE gathering state: {state:?}");
        if matches!(state, GatheringState::Complete) {
            let shared = self.shared.lock().unwrap();
            if let Err(e) = shared.event_tx.try_send(PeerEvent::GatheringComplete) {
                warn!("gathering complete event dropped — channel full: {e}");
            }
        }
    }

    fn on_data_channel(&mut self, dc: Box<RtcDataChannel<Self::DCH>>) {
        let label = dc.label();
        debug!("incoming data channel: {label}");

        // Get the send_rx from shared state (stored by data_channel_handler)
        let send_rx = {
            let mut shared = self.shared.lock().unwrap();
            shared.incoming_send_rx.remove(&label)
        };

        if let Some(mut send_rx) = send_rx {
            let dc_arc = Arc::new(std::sync::Mutex::new(Some(dc)));
            let dc_send = Arc::clone(&dc_arc);

            // Get the alive flag for this channel from shared state
            let alive_flag = {
                let shared = self.shared.lock().unwrap();
                shared.incoming_alive_flags.get(&label).cloned()
            };

            // Spawn send loop in a background thread (datachannel callbacks
            // are on libdatachannel's thread, can't use tokio::spawn)
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    while let Some(frame) = send_rx.recv().await {
                        let result = {
                            let mut guard = dc_send.lock().unwrap();
                            if let Some(dc) = guard.as_mut() {
                                dc.send(&frame)
                            } else {
                                break;
                            }
                        };
                        if let Err(e) = result {
                            warn!("incoming channel send error: {e}");
                            if let Some(ref flag) = alive_flag {
                                flag.store(false, std::sync::atomic::Ordering::Release);
                            }
                            break;
                        }
                    }
                });
            });
        } else {
            // No send_rx — keep dc alive but can't send back.
            // Store in SharedState so it gets dropped with the peer.
            let dc_arc = Arc::new(std::sync::Mutex::new(Some(dc)));
            let mut shared = self.shared.lock().unwrap();
            shared.incoming_channels.push(dc_arc);
        }
    }
}

/// Data channel handler — bridges callbacks to tokio channels.
struct B3DcHandler {
    label: String,
    decoded_tx: mpsc::Sender<ChannelMessage>,
    assembler: Option<ChunkAssembler>,
}

impl DataChannelHandler for B3DcHandler {
    fn on_open(&mut self) {
        info!("data channel '{}' opened", self.label);
    }

    fn on_closed(&mut self) {
        debug!("data channel '{}' closed", self.label);
    }

    fn on_error(&mut self, err: &str) {
        error!("data channel '{}' error: {err}", self.label);
    }

    fn on_message(&mut self, msg: &[u8]) {
        // Pass through b3-reliable frames as Binary without decoding.
        // The GPU relay sends reliable-framed data over RTC data channels;
        // these have 0xB3 magic bytes that aren't valid ChannelMessage types.
        if crate::channel::is_b3_reliable_frame(msg) {
            if let Err(e) = self.decoded_tx.try_send(ChannelMessage::Binary(msg.to_vec())) {
                warn!("reliable frame dropped on '{}' — channel full: {e}", self.label);
            }
            return;
        }

        match ChannelMessage::decode(msg) {
            Ok(DecodeResult::Complete(channel_msg)) => {
                if let Err(e) = self.decoded_tx.try_send(channel_msg) {
                    warn!("decoded message dropped on '{}' — channel full: {e}", self.label);
                }
            }
            Ok(DecodeResult::Chunk {
                seq,
                total,
                original_type,
                data,
            }) => {
                let assembler = self
                    .assembler
                    .get_or_insert_with(|| ChunkAssembler::new(total, original_type));
                if let Some(complete_msg) = assembler.feed(seq, data) {
                    self.assembler = None;
                    if let Err(e) = self.decoded_tx.try_send(complete_msg) {
                        warn!("assembled message dropped on '{}' — channel full: {e}", self.label);
                    }
                }
            }
            Err(e) => {
                warn!("decode error on '{}': {e}", self.label);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_config_default() {
        let config = PeerConfig::default();
        assert!(!config.force_relay);
        assert!(!config.ice_servers.is_empty());
    }
}
