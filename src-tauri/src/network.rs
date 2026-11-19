use crate::bitrate::{
    BitrateController, BitrateDecision, CongestionState, DEFAULT_INITIAL_BITRATE_BPS,
    MAX_VIDEO_BITRATE_BPS, PathTelemetrySample,
};
use crate::connections::{create_json, read_json};
use crate::protocol::{
    AudioPacket, ChannelId, ControlMessage, InputPacket, PROTOCOL_VERSION, ReassembleOutcome,
    ReassembledFrame, StreamKind, VideoCodec, VideoReassembler, packetize_video,
};
use crate::signaling::{
    ParsedInvite, RelayConnection, SignalingError, build_invite_uri, computer_display_name,
    discover_lan_signaling, generate_room_code, is_fingerprint, is_lan_signaling_url,
    parse_invite_input, signal_as_client, signal_as_host, validate_signaling_url,
};
use crate::stun::{IceCandidates, StunError, canonical_addr, gather_candidates, socket_target};
use crate::transport::QUIC_DATAGRAM_BUFFER_BYTES;
use bytes::{BufMut, Bytes, BytesMut};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{interval, timeout};
use tracing::{debug, info};

const VMAX_ALPN: &[u8] = b"vmax/1";
const PUNCH_MAGIC: &[u8] = b"VMAX_PUNCH_V1";
const PUNCH_ACK_MAGIC: &[u8] = b"VMAX_PUNCH_ACK_V1";
const DEFAULT_FEC_GROUP_SIZE: u8 = 10;
const MAX_QUEUED_DELTA_BYTES: usize = 512 * 1024;
const RELAY_STARTUP_BITRATE_BPS: u32 = 4_000_000;
const HIGH_RTT_STARTUP_BITRATE_BPS: u32 = 6_000_000;
/// Maximum age of an encoded delta frame before it is considered stale.
///
/// Scale this with the negotiated frame rate instead of using a fixed 30 ms:
/// at 120/144 FPS a 30 ms queue is 3-4 frames of avoidable latency. Two frame
/// intervals keeps enough scheduler slack without allowing a deep backlog.
fn max_capture_queue_age(fps: u16) -> Duration {
    (Duration::from_secs_f64(2.0 / f64::from(fps.max(1))))
        .clamp(Duration::from_millis(12), Duration::from_millis(30))
}
const LAN_PROBE_HEAD_START: Duration = Duration::from_millis(125);
const NETWORK_RTT_TARGET: Duration = Duration::from_millis(35);

#[derive(Default)]
struct VideoRecovery {
    awaiting_keyframe: bool,
    last_request: Option<Instant>,
}

impl VideoRecovery {
    fn request_keyframe(
        &mut self,
        events: &mpsc::UnboundedSender<NetworkEvent>,
        reason: &'static str,
    ) {
        self.awaiting_keyframe = true;
        let now = Instant::now();
        if self
            .last_request
            .is_some_and(|last| now - last < Duration::from_millis(300))
        {
            return;
        }
        self.last_request = Some(now);
        // Intra refresh is carried in delta frames. The queue cannot use it
        // to restart after dropping a reference frame.
        let _ = events.send(NetworkEvent::KeyframeRequested {
            last_good_frame_id: None,
            reason: reason.into(),
        });
    }

    fn accepted_keyframe(&mut self) {
        self.awaiting_keyframe = false;
        self.last_request = None;
    }
}

fn pending_video_cap(
    target_bps: usize,
    fps: u16,
    rtt: Duration,
    direct: bool,
    recovery_allowance: Duration,
) -> usize {
    // Allow at least 64 KB (a single detailed P-frame on camera pans) so Wi-Fi jitter
    // doesn't trigger false keyframe spirals.
    let frame_time = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    if !direct {
        // A TCP relay already has its own queue. Reassembly slack belongs on the
        // receiver only; adding it here lets stale video sit in front of input.
        let budget = (frame_time * 2).clamp(Duration::from_millis(16), Duration::from_millis(32));
        return ((target_bps as f64 / 8.0 * budget.as_secs_f64()) as usize)
            .clamp(12 * 1024, MAX_QUEUED_DELTA_BYTES);
    }
    let budget = NETWORK_RTT_TARGET
        .saturating_sub(rtt / 2)
        .max(frame_time)
        .min(Duration::from_millis(40))
        + recovery_allowance;
    ((target_bps as f64 / 8.0 * budget.as_secs_f64()) as usize)
        .clamp(64 * 1024, MAX_QUEUED_DELTA_BYTES)
}

fn quic_startup_bitrate(initial_bps: u32, relayed: bool, rtt: Duration) -> u32 {
    if relayed {
        initial_bps.min(RELAY_STARTUP_BITRATE_BPS)
    } else if rtt >= Duration::from_millis(60) {
        initial_bps.min(HIGH_RTT_STARTUP_BITRATE_BPS)
    } else {
        initial_bps
    }
}

fn is_local_candidate(address: SocketAddr) -> bool {
    match canonical_addr(address).ip() {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback()
        }
    }
}

fn bind_media_socket(address: SocketAddr, public_signaling: bool) -> io::Result<StdUdpSocket> {
    let socket = if public_signaling && address.is_ipv4() && address.ip().is_unspecified() {
        let dual_stack = || -> io::Result<StdUdpSocket> {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_only_v6(false)?;
            socket.bind(&SocketAddr::from(([0; 8], address.port())).into())?;
            Ok(socket.into())
        };
        match dual_stack() {
            Ok(socket) => socket,
            Err(_) => StdUdpSocket::bind(address)?,
        }
    } else {
        StdUdpSocket::bind(address)?
    };
    // 4 MiB send/recv buffers + WMM/DSCP EF.  DSCP is best-effort; the
    // buffer sizes are required for the pacer on weak Wi-Fi.
    crate::transport::tune_udp_socket(&socket)?;
    Ok(socket)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPath {
    DirectLan,
    DirectInternet,
    Relay,
}

impl ConnectionPath {
    fn direct(peer: SocketAddr) -> Self {
        if is_local_candidate(peer) {
            Self::DirectLan
        } else {
            Self::DirectInternet
        }
    }
}

fn keyframe_is_draining(sent: Option<Instant>) -> bool {
    // A single large keyframe may briefly fill the queue on an otherwise healthy path.
    sent.is_some_and(|instant| instant.elapsed() < Duration::from_millis(100))
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("STUN error: {0}")]
    Stun(#[from] StunError),
    #[error("signaling error: {0}")]
    Signaling(#[from] SignalingError),
    #[error("Signaling ({endpoint_source}): {source}")]
    SignalingRoute {
        endpoint_source: &'static str,
        #[source]
        source: SignalingError,
    },
    #[error(
        "No host found for that room code on your local network. On the host PC, use Start internet hosting for a client on a different network. Then copy the full invite link and paste it into Join Stream."
    )]
    LocalRoomNotFound,
    #[error("protocol error: {0}")]
    Protocol(#[from] crate::protocol::ProtocolError),
    #[error("TLS certificate error: {0}")]
    Tls(String),
    #[error("QUIC connection failed: {0}")]
    QuicConnect(#[from] quinn::ConnectError),
    #[error("QUIC transport error: {0}")]
    QuicConnection(#[from] quinn::ConnectionError),
    #[error(
        "Direct UDP is unreachable. Select QUIC and use a VmaX relay endpoint on networks that block UDP."
    )]
    HolePunchTimeout,
    #[error("Relay-only hosting requires a VmaX server that supports media relay")]
    RelayUnavailable,
    #[error("QUIC handshake did not complete")]
    HandshakeTimeout,
    #[error("saved connection rejected: {0}")]
    Permission(String),
    #[error("session cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Host,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase {
    Idle,
    DiscoveringNat,
    WaitingForPeer,
    HolePunching,
    QuicHandshake,
    Connected,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportProtocol {
    Quic,
    RawUdp,
}

/// Congestion control sensitivity profile.
/// Matches `congestion::GccController::Sensitivity`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CongestionSensitivity {
    Balanced,
    LowLatency,
    Quality,
}

impl Default for CongestionSensitivity {
    fn default() -> Self {
        Self::Balanced
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub signaling_url: String,
    #[serde(default)]
    pub advertised_signaling_url: Option<String>,
    pub stun_servers: Vec<String>,
    pub enable_stun: bool,
    #[serde(default)]
    pub relay_only: bool,
    pub bind_addr: SocketAddr,
    pub max_bitrate_bps: u32,
    pub initial_bitrate_bps: u32,
    #[serde(default)]
    pub fixed_bitrate: bool,
    pub fec_group_size: u8,
    pub protocol: TransportProtocol,
    pub target_fps: u16,
    pub jitter_window_ms: u64,
    pub hole_punch_timeout_ms: u64,
    #[serde(default)]
    pub congestion_sensitivity: CongestionSensitivity,
    #[serde(skip)]
    pub identity_file: Option<PathBuf>,
    #[serde(skip)]
    pub profile_endpoint: Option<String>,
    #[serde(skip)]
    pub saved_clients: Option<Vec<String>>,
    #[serde(skip)]
    pub invited_client: Option<String>,
    #[serde(skip)]
    pub host_room_code: Option<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            signaling_url: "ws://127.0.0.1:8787".to_string(),
            advertised_signaling_url: None,
            stun_servers: vec![
                "stun.l.google.com:19302".to_string(),
                "stun.cloudflare.com:3478".to_string(),
            ],
            enable_stun: true,
            relay_only: false,
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            max_bitrate_bps: MAX_VIDEO_BITRATE_BPS,
            initial_bitrate_bps: DEFAULT_INITIAL_BITRATE_BPS,
            fixed_bitrate: false,
            fec_group_size: DEFAULT_FEC_GROUP_SIZE,
            protocol: TransportProtocol::Quic,
            target_fps: 60,
            // Only incomplete frames wait this long; complete frames play
            // immediately. A window of ~2 frame intervals at 60 Hz absorbs
            // ordinary Wi-Fi/cellular jitter without adding latency.
            jitter_window_ms: 32,
            hole_punch_timeout_ms: 4_500,
            congestion_sensitivity: CongestionSensitivity::Balanced,
            identity_file: None,
            profile_endpoint: None,
            saved_clients: None,
            invited_client: None,
            host_room_code: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkTelemetry {
    pub role: Option<SessionRole>,
    pub phase: ConnectionPhase,
    pub room_code: Option<String>,
    pub invite_uri: Option<String>,
    pub local_udp_addr: Option<SocketAddr>,
    pub public_reflexive_addr: Option<SocketAddr>,
    pub active_peer_addr: Option<SocketAddr>,
    #[serde(default)]
    pub relayed: bool,
    #[serde(default)]
    pub connection_path: Option<ConnectionPath>,
    #[serde(default)]
    pub saved_session: bool,
    #[serde(default)]
    pub peer_savable: bool,
    #[serde(skip)]
    pub(crate) trusted_peer: Option<TrustedPeer>,
    pub cert_fingerprint: Option<String>,
    pub rtt_ms: f64,
    pub min_rtt_ms: f64,
    pub packet_loss_pct: f64,
    pub target_bitrate_bps: u32,
    pub congestion_state: CongestionState,
    pub video_frames_tx: u64,
    pub video_frames_rx: u64,
    pub video_frames_dropped: u64,
    pub fec_recovered_shards: u64,
    pub audio_packets_tx: u64,
    pub audio_packets_rx: u64,
    #[serde(skip)]
    pub(crate) media_bitrate_mbps: f32,
    pub input_events_tx: u64,
    pub input_events_rx: u64,
    pub last_error: Option<String>,
}

impl Default for NetworkTelemetry {
    fn default() -> Self {
        Self {
            role: None,
            phase: ConnectionPhase::Idle,
            room_code: None,
            invite_uri: None,
            local_udp_addr: None,
            public_reflexive_addr: None,
            active_peer_addr: None,
            relayed: false,
            connection_path: None,
            saved_session: false,
            peer_savable: false,
            trusted_peer: None,
            cert_fingerprint: None,
            rtt_ms: 0.0,
            min_rtt_ms: 0.0,
            packet_loss_pct: 0.0,
            target_bitrate_bps: DEFAULT_INITIAL_BITRATE_BPS,
            congestion_state: CongestionState::Stable,
            video_frames_tx: 0,
            video_frames_rx: 0,
            video_frames_dropped: 0,
            fec_recovered_shards: 0,
            audio_packets_tx: 0,
            audio_packets_rx: 0,
            media_bitrate_mbps: 0.0,
            input_events_tx: 0,
            input_events_rx: 0,
            last_error: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedPeer {
    pub fingerprint: String,
    pub endpoint: String,
    pub resume_key: Option<String>,
}

impl std::fmt::Debug for TrustedPeer {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("TrustedPeer")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// Video access unit handed to the sender from the encoder.
#[derive(Debug, Clone)]
pub struct OutboundVideoFrame {
    /// Encode timing and stream description repeated in every datagram of the
    /// frame, so the client can render host-side latency without an extra
    /// control round trip.
    pub meta: crate::telemetry::FrameTelemetry,
    pub codec: VideoCodec,
    pub frame_id: u32,
    pub timestamp_us: u64,
    pub captured_at: Instant,
    pub fps: u16,
    pub is_keyframe: bool,
    pub payload: Bytes,
}

/// Events emitted by the P2P network loop toward capture, audio, input, and UI layers.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    PhaseChanged(ConnectionPhase),
    InviteReady {
        room_code: String,
        invite_uri: String,
    },
    VideoFrameReceived(ReassembledFrame),
    AudioPacketReceived(AudioPacket),
    InputEventReceived(InputPacket),
    RumbleReceived(crate::haptics::Rumble),
    KeyframeRequested {
        last_good_frame_id: Option<u32>,
        reason: String,
    },
    /// Ask the encoder to start a rolling intra-refresh wave.  This is the
    /// normal loss-recovery path and deliberately avoids an IDR spike.
    IntraRefreshRequested {
        frames: u32,
        reason: String,
    },
    BitrateUpdated(BitrateDecision),
    ControlReceived(ControlMessage),
    /// Client ACK / delay-gradient report.  Feed this into
    /// `congestion::GccController` on the host; client-side loops can ignore it.
    CongestionFeedback(Box<crate::congestion::FeedbackPacket>),
    TelemetryUpdated(Box<NetworkTelemetry>),
}

#[derive(Clone)]
struct RuntimeOverrides {
    fec_group_size: Arc<AtomicU8>,
    bitrate_policy: Arc<AtomicU32>,
    client_limit_bps: Arc<AtomicU32>,
    /// Shared with the host capture loop, which reads it to slow down before a
    /// frame has to be dropped.
    backlog: VideoBacklog,
}

impl RuntimeOverrides {
    fn bitrate_policy(&self) -> (u32, bool) {
        // One atomic keeps the configured target and fixed-mode flag in sync.
        let policy = self.bitrate_policy.load(Ordering::Relaxed);
        let host_max = policy >> 1;
        let client_max = self.client_limit_bps.load(Ordering::Relaxed);
        let ceiling = if client_max == 0 {
            host_max
        } else {
            host_max.min(client_max)
        };
        (ceiling, policy & 1 != 0)
    }

    fn target_bitrate(&self, adaptive_bps: u32) -> u32 {
        let (ceiling, fixed) = self.bitrate_policy();
        if fixed {
            ceiling
        } else {
            adaptive_bps.min(ceiling)
        }
    }

    fn apply_bitrate_policy(&self, decision: &mut BitrateDecision) {
        let (ceiling, fixed) = self.bitrate_policy();
        if fixed {
            decision.target_bitrate_bps = ceiling;
            decision.reason = "fixed bitrate".into();
        } else {
            decision.target_bitrate_bps = decision.target_bitrate_bps.min(ceiling);
        }
    }
}

/// Live view of the video send queue, shared with the host capture loop.
///
/// A queue is the only place a frame can be discarded without hurting the
/// stream, and only *before* it is encoded. Dropping an encoded delta leaves a
/// hole in the reference chain, which forces an IDR and turns a saturated
/// uplink into a visible freeze followed by a bitrate spike. Publishing the
/// queue lets the capture loop skip one slot instead: that costs a frame of
/// cadence and nothing else.
///
/// The pacer side is read directly from its own counter, so the value never
/// goes stale when frames stop flowing. Only the QUIC-side contribution has to
/// be published, because only the sender may ask the connection.
#[derive(Debug, Clone, Default)]
pub struct VideoBacklog {
    pacer: Arc<crate::transport::PacerStats>,
    quic_bytes: Arc<AtomicU64>,
    cap_bytes: Arc<AtomicU64>,
}

impl VideoBacklog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pacer's statistics object, so `PacedSender::spawn_with_stats` can
    /// share its live queue with this view.
    pub fn pacer_stats(&self) -> Arc<crate::transport::PacerStats> {
        Arc::clone(&self.pacer)
    }

    /// Bytes queued between the pacer and the wire.
    pub fn queued_bytes(&self) -> u64 {
        self.pacer.queued_bytes() + self.quic_bytes.load(Ordering::Relaxed)
    }

    /// The ceiling above which the sender drops a frame.
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes.load(Ordering::Relaxed)
    }

    /// True when another encoded frame would not fit.
    pub fn saturated(&self) -> bool {
        let cap = self.cap_bytes();
        cap > 0 && self.queued_bytes() >= cap
    }

    fn note_quic_bytes(&self, bytes: u64) {
        self.quic_bytes.store(bytes, Ordering::Relaxed);
    }

    fn set_cap_bytes(&self, bytes: usize) {
        self.cap_bytes.store(bytes as u64, Ordering::Relaxed);
    }
}

/// Non-blocking sender handles for pushing video, audio, input, and control into the active QUIC loop.
#[derive(Clone)]
pub struct SessionTx {
    pub video_tx: mpsc::Sender<OutboundVideoFrame>,
    pub audio_tx: mpsc::Sender<AudioPacket>,
    pub input_tx: mpsc::Sender<InputPacket>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub haptics_tx: watch::Sender<crate::haptics::Rumble>,
    shutdown_tx: watch::Sender<bool>,
    overrides: RuntimeOverrides,
}

impl SessionTx {
    pub fn close(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub fn set_fec_group_size(&self, group_size: u8) {
        self.overrides
            .fec_group_size
            .store(group_size, Ordering::Relaxed);
    }

    pub fn set_bitrate_policy(&self, max_bps: u32, fixed: bool) {
        self.overrides
            .bitrate_policy
            .store((max_bps << 1) | u32::from(fixed), Ordering::Relaxed);
    }

    pub fn set_client_bitrate_limit(&self, max_bps: u32) {
        self.overrides
            .client_limit_bps
            .store(max_bps, Ordering::Relaxed);
    }

    /// Live view of the video send queue. The host capture thread holds one to
    /// stop encoding frames the wire cannot carry yet.
    pub fn video_backlog(&self) -> VideoBacklog {
        self.overrides.backlog.clone()
    }
}

pub struct SessionHandle {
    pub tx: SessionTx,
    pub event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    pub telemetry: Arc<Mutex<NetworkTelemetry>>,
    pub join_handle: tokio::task::JoinHandle<Result<(), NetworkError>>,
}

struct EphemeralIdentity {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    fingerprint_hex: String,
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    certificate: Vec<u8>,
    private_key: Vec<u8>,
}

pub(crate) fn identity_fingerprint(path: PathBuf) -> Result<String, NetworkError> {
    Ok(load_identity(Some(path))?.fingerprint_hex)
}

fn load_identity(path: Option<PathBuf>) -> Result<EphemeralIdentity, NetworkError> {
    let Some(path) = path else {
        return generate_ephemeral_identity();
    };
    fn from_stored(stored: StoredIdentity) -> EphemeralIdentity {
        EphemeralIdentity {
            fingerprint_hex: hex::encode(Sha256::digest(&stored.certificate)),
            cert_der: CertificateDer::from(stored.certificate),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(stored.private_key)),
        }
    }
    if let Some(stored) = read_json::<StoredIdentity>(&path)? {
        return Ok(from_stored(stored));
    }
    let identity = generate_ephemeral_identity()?;
    match create_json(
        &path,
        &StoredIdentity {
            certificate: identity.cert_der.as_ref().to_vec(),
            private_key: identity.key_der.secret_der().to_vec(),
        },
    ) {
        Ok(()) => Ok(identity),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_json(&path)?
            .map(from_stored)
            .ok_or_else(|| error.into()),
        Err(error) => Err(error.into()),
    }
}

fn host_resume_key(identity: &EphemeralIdentity) -> String {
    let mut hash = Sha256::new();
    hash.update(b"vmax-saved-rendezvous-v1");
    hash.update(identity.key_der.secret_der());
    hex::encode(hash.finalize())
}

pub(crate) async fn friend_relay_command(
    identity_path: PathBuf,
    endpoint: &str,
    command: crate::signaling::FriendCommand,
) -> Result<(Vec<String>, bool), NetworkError> {
    use crate::signaling::{SignalingMessage, connect_signaling, friend_proof_payload};
    use futures_util::{SinkExt, StreamExt};
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair},
    };
    use tokio_tungstenite::tungstenite::Message;

    crate::signaling::ensure_rustls_provider();
    let identity = load_identity(Some(identity_path))?;
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        identity.key_der.secret_der(),
        &rng,
    )
    .map_err(|_| NetworkError::Tls("Friend identity key could not be loaded".into()))?;
    let mut socket = connect_signaling(endpoint).await?;
    let exchange = async {
        socket
            .send(Message::Text(
                serde_json::to_string(&SignalingMessage::FriendCommand {
                    certificate: identity.cert_der.as_ref().to_vec(),
                    command: command.clone(),
                })
                .map_err(SignalingError::from)?
                .into(),
            ))
            .await
            .map_err(SignalingError::from)?;
        let Some(Ok(Message::Text(challenge))) = socket.next().await else {
            return Err(SignalingError::RemoteError(
                "This relay needs an update to support friend requests.".into(),
            )
            .into());
        };
        let SignalingMessage::FriendChallenge { nonce } =
            serde_json::from_str(&challenge).map_err(SignalingError::from)?
        else {
            return Err(SignalingError::RemoteError(
                "This relay needs an update to support friend requests.".into(),
            )
            .into());
        };
        let signature = key
            .sign(&rng, &friend_proof_payload(&nonce, &command)?)
            .map_err(|_| NetworkError::Tls("Could not sign friend request".into()))?;
        socket
            .send(Message::Text(
                serde_json::to_string(&SignalingMessage::FriendProof {
                    signature: signature.as_ref().to_vec(),
                })
                .map_err(SignalingError::from)?
                .into(),
            ))
            .await
            .map_err(SignalingError::from)?;
        let Some(Ok(Message::Text(reply))) = socket.next().await else {
            return Err(SignalingError::Closed.into());
        };
        match serde_json::from_str(&reply).map_err(SignalingError::from)? {
            SignalingMessage::FriendReply { requests, approved } => Ok((requests, approved)),
            SignalingMessage::Error { message } => Err(SignalingError::RemoteError(message).into()),
            _ => Err(SignalingError::Closed.into()),
        }
    };
    timeout(Duration::from_secs(12), exchange)
        .await
        .map_err(|_| SignalingError::TimedOut)?
}

fn generate_ephemeral_identity() -> Result<EphemeralIdentity, NetworkError> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["vmax.peer".to_string(), "localhost".to_string()])
            .map_err(|e| NetworkError::Tls(e.to_string()))?;

    let cert_bytes = cert.der().to_vec();
    let fingerprint_hex = hex::encode(Sha256::digest(&cert_bytes));
    let cert_der = CertificateDer::from(cert_bytes);
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

    Ok(EphemeralIdentity {
        cert_der,
        key_der,
        fingerprint_hex,
    })
}

/// Verifies the peer's TLS 1.3 leaf certificate against the SHA-256 fingerprint exchanged over signaling.
#[derive(Debug)]
struct FingerprintCertVerifier {
    expected_fingerprint_hex: String,
    supported_algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl FingerprintCertVerifier {
    fn new(expected_fingerprint_hex: String) -> Self {
        let provider = rustls::crypto::ring::default_provider();
        Self {
            expected_fingerprint_hex: expected_fingerprint_hex.to_lowercase(),
            supported_algs: provider.signature_verification_algorithms,
        }
    }

    fn verify_fingerprint(&self, cert: &CertificateDer<'_>) -> Result<(), TlsError> {
        if hex::encode(Sha256::digest(cert.as_ref())) == self.expected_fingerprint_hex {
            Ok(())
        } else {
            Err(TlsError::InvalidCertificate(CertificateError::BadSignature))
        }
    }
}

impl ServerCertVerifier for FingerprintCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.verify_fingerprint(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

impl ClientCertVerifier for FingerprintCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        _chain: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        self.verify_fingerprint(cert)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        ServerCertVerifier::verify_tls12_signature(self, message, cert, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        ServerCertVerifier::verify_tls13_signature(self, message, cert, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ServerCertVerifier::supported_verify_schemes(self)
    }
}

fn build_transport_config() -> quinn::TransportConfig {
    // Bounded DPLPMTUD, a 200 ms keepalive so CGNAT mappings never expire, and
    // automatic QUIC connection migration when the phone changes Wi-Fi <->
    // cellular.  See `wan_transport` for the carrier-MTU reasoning.
    crate::wan_transport::wan_transport_config(crate::wan_transport::WanLink::Cellular)
}

fn build_server_config(
    identity: &EphemeralIdentity,
    client_fp: Option<&str>,
) -> Result<ServerConfig, NetworkError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetworkError::Tls(e.to_string()))?;
    let tls = match client_fp {
        Some(fp) => {
            tls.with_client_cert_verifier(Arc::new(FingerprintCertVerifier::new(fp.to_string())))
        }
        None => tls.with_no_client_auth(),
    };
    let mut tls = tls
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .map_err(|e| NetworkError::Tls(e.to_string()))?;

    tls.alpn_protocols = vec![VMAX_ALPN.to_vec()];
    let quic_tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| NetworkError::Tls(e.to_string()))?;

    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_tls));
    server_cfg.migration(true);
    server_cfg.transport_config(Arc::new(build_transport_config()));
    Ok(server_cfg)
}

fn build_client_config(
    expected_peer_fingerprint: &str,
    identity: Option<&EphemeralIdentity>,
) -> Result<ClientConfig, NetworkError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetworkError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FingerprintCertVerifier::new(
            expected_peer_fingerprint.to_string(),
        )));
    let mut tls = match identity {
        Some(identity) => tls
            .with_client_auth_cert(
                vec![identity.cert_der.clone()],
                identity.key_der.clone_key(),
            )
            .map_err(|err| NetworkError::Tls(err.to_string()))?,
        None => tls.with_no_client_auth(),
    };

    tls.alpn_protocols = vec![VMAX_ALPN.to_vec()];
    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| NetworkError::Tls(e.to_string()))?;

    let mut client_cfg = ClientConfig::new(Arc::new(quic_tls));
    client_cfg.transport_config(Arc::new(build_transport_config()));
    Ok(client_cfg)
}

/// Performs simultaneous UDP hole-punching on the bound socket before handing the socket to Quinn.
async fn hole_punch_udp(
    socket: &UdpSocket,
    peer_candidates: &[SocketAddr],
    punch_timeout: Duration,
    tag: Option<[u8; 32]>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<SocketAddr, NetworkError> {
    if peer_candidates.is_empty() {
        return Err(NetworkError::HolePunchTimeout);
    }

    let started_at = Instant::now();
    let deadline = started_at + punch_timeout;
    let try_public_at = if peer_candidates.iter().copied().any(is_local_candidate) {
        started_at + LAN_PROBE_HEAD_START.min(punch_timeout / 3)
    } else {
        started_at
    };
    // Aggressive-then-backoff punch cadence (Parsec-style): a 10 ms burst for
    // the first 250 ms, then ICE-like relaxation out to 100 ms.  This is what
    // gets through NATs that need a rapid two-way exchange, while a long 4.5 s
    // window still costs almost nothing on a phone radio.
    let mut punch_schedule = crate::connectivity::PunchSchedule::new();
    let mut next_punch = Instant::now();
    let ipv6_socket = socket.local_addr()?.is_ipv6();
    let mut recv_buf = [0u8; 2048];
    let mut confirmed_addr: Option<SocketAddr> = None;
    let mut first_probe = std::collections::HashMap::new();
    let mut best_ack: Option<(Duration, SocketAddr)> = None;
    let mut selection_deadline = deadline;
    let mut punch = PUNCH_MAGIC.to_vec();
    let mut ack = PUNCH_ACK_MAGIC.to_vec();
    if let Some(tag) = tag {
        punch.extend_from_slice(&tag);
        ack.extend_from_slice(&tag);
    }

    loop {
        if *shutdown_rx.borrow() {
            return Err(NetworkError::Cancelled);
        }
        if Instant::now() >= selection_deadline {
            if let Some(addr) = best_ack.map(|(_, addr)| addr).or(confirmed_addr) {
                let _ = socket.send_to(&ack, addr).await;
                return Ok(addr);
            }
            return Err(NetworkError::HolePunchTimeout);
        }

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Err(NetworkError::Cancelled);
                }
            }
            _ = tokio::time::sleep_until(next_punch.into()) => {
                let (interval, burst) = punch_schedule.next(started_at.elapsed());
                next_punch = Instant::now() + interval;
                if let Some(target) = confirmed_addr {
                    let _ = socket.send_to(&ack, target).await;
                }
                if confirmed_addr.is_none() || tag.is_some() {
                    let allow_public = Instant::now() >= try_public_at;
                    for _ in 0..burst {
                        for &candidate in peer_candidates.iter().take(16) {
                            if allow_public || is_local_candidate(candidate) {
                                first_probe.entry(candidate).or_insert_with(Instant::now);
                                let _ = socket
                                    .send_to(&punch, socket_target(candidate, ipv6_socket))
                                    .await;
                            }
                        }
                    }
                }
            }
            recv = socket.recv_from(&mut recv_buf) => {
                match recv {
                    Ok((len, from)) => {
                        let data = &recv_buf[..len];
                        let canonical_from = canonical_addr(from);
                        let known_ip = peer_candidates.iter().any(|candidate| canonical_addr(*candidate).ip() == canonical_from.ip());
                        if data == ack && (tag.is_some() || known_ip) {
                            let response = first_probe.get(&canonical_from).map_or(Duration::MAX, Instant::elapsed);
                            if tag.is_none() || response <= NETWORK_RTT_TARGET {
                                for _ in 0..3 {
                                    let _ = socket.send_to(&ack, from).await;
                                }
                                return Ok(from);
                            }
                            if best_ack.is_none() {
                                selection_deadline = deadline.min(Instant::now() + NETWORK_RTT_TARGET);
                            }
                            if best_ack.is_none_or(|(best, _)| response < best) {
                                best_ack = Some((response, from));
                            }
                        } else if data == punch && (tag.is_some() || known_ip) {
                            confirmed_addr = Some(from);
                            for _ in 0..3 {
                                let _ = socket.send_to(&ack, from).await;
                            }
                        } else if tag.is_none() && !data.is_empty()
                            && ((data[0] & 0x80) != 0
                                || data[0] == ChannelId::Control as u8
                                || data[0] == ChannelId::Video as u8)
                            && known_ip
                        {
                            return Ok(from);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                    Err(e) => return Err(NetworkError::Io(e)),
                }
            }
        }
    }
}

pub async fn start_host_session(config: NetworkConfig) -> Result<SessionHandle, NetworkError> {
    spawn_session(SessionRole::Host, config, None).await
}

pub async fn start_client_session(
    mut config: NetworkConfig,
    invite_input: String,
) -> Result<SessionHandle, NetworkError> {
    let parsed = parse_invite_input(&invite_input, &config.signaling_url)?;
    if let Some(raw_udp) = parsed.raw_udp {
        config.protocol = if raw_udp {
            TransportProtocol::RawUdp
        } else {
            TransportProtocol::Quic
        };
    }
    spawn_session(SessionRole::Client, config, Some(parsed)).await
}

pub async fn start_saved_client_session(
    mut config: NetworkConfig,
    fingerprint: String,
    resume_key: String,
) -> Result<SessionHandle, NetworkError> {
    if !is_fingerprint(&fingerprint)
        || !is_fingerprint(&resume_key)
        || config.identity_file.is_none()
    {
        return Err(NetworkError::Permission(
            "a verified device identity is required".into(),
        ));
    }
    let url = validate_signaling_url(&config.signaling_url)?;
    let invite = ParsedInvite {
        room_code: format!("saved-{resume_key}"),
        signaling_url: url.to_string(),
        expected_host_fingerprint: Some(fingerprint),
        raw_udp: Some(false),
        discover_lan: false,
        endpoint_from_link: false,
    };
    config.protocol = TransportProtocol::Quic;
    spawn_session(SessionRole::Client, config, Some(invite)).await
}

async fn spawn_session(
    role: SessionRole,
    mut config: NetworkConfig,
    invite: Option<ParsedInvite>,
) -> Result<SessionHandle, NetworkError> {
    crate::signaling::ensure_rustls_provider();

    if let Some(ref inv) = invite {
        config.signaling_url = inv.signaling_url.clone();
        config.profile_endpoint = Some(validate_signaling_url(&inv.signaling_url)?.to_string());
    }

    let saved_session = config.saved_clients.is_some()
        || invite
            .as_ref()
            .is_some_and(|invite| invite.room_code.starts_with("saved-"));
    let (video_tx, video_rx) = mpsc::channel::<OutboundVideoFrame>(2);
    // Keep realtime queues shallow. Audio/input producers already use try_send
    // for high-frequency traffic, so large queues only preserve stale data.
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(8);
    let (input_tx, input_rx) = mpsc::channel::<InputPacket>(32);
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(64);
    let (haptics_tx, haptics_rx) = watch::channel(crate::haptics::Rumble::default());
    let (event_tx, event_rx) = mpsc::unbounded_channel::<NetworkEvent>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let telemetry = Arc::new(Mutex::new(NetworkTelemetry {
        role: Some(role),
        saved_session,
        target_bitrate_bps: config.initial_bitrate_bps,
        ..Default::default()
    }));
    let telemetry_ref = Arc::clone(&telemetry);

    let overrides = RuntimeOverrides {
        fec_group_size: Arc::new(AtomicU8::new(config.fec_group_size)),
        bitrate_policy: Arc::new(AtomicU32::new(
            (config.max_bitrate_bps << 1) | u32::from(config.fixed_bitrate),
        )),
        client_limit_bps: Arc::new(AtomicU32::new(0)),
        backlog: VideoBacklog::new(),
    };
    let loop_overrides = overrides.clone();

    let tx_handle = SessionTx {
        video_tx,
        audio_tx,
        input_tx,
        control_tx,
        haptics_tx,
        shutdown_tx,
        overrides,
    };

    let join_handle = tokio::spawn(async move {
        let res = run_p2p_session(
            role,
            config,
            invite,
            video_rx,
            audio_rx,
            input_rx,
            control_rx,
            haptics_rx,
            event_tx.clone(),
            Arc::clone(&telemetry_ref),
            shutdown_rx,
            loop_overrides,
        )
        .await;

        if let Err(ref err) = res {
            let mut guard = telemetry_ref.lock().await;
            guard.phase = ConnectionPhase::Failed;
            guard.last_error = Some(err.to_string());
            let _ = event_tx.send(NetworkEvent::PhaseChanged(ConnectionPhase::Failed));
            let _ = event_tx.send(NetworkEvent::TelemetryUpdated(Box::new(guard.clone())));
        }
        res
    });

    Ok(SessionHandle {
        tx: tx_handle,
        event_rx,
        telemetry,
        join_handle,
    })
}

async fn set_phase(
    phase: ConnectionPhase,
    telemetry: &Arc<Mutex<NetworkTelemetry>>,
    event_tx: &mpsc::UnboundedSender<NetworkEvent>,
) {
    let snapshot = {
        let mut guard = telemetry.lock().await;
        guard.phase = phase;
        guard.clone()
    };
    let _ = event_tx.send(NetworkEvent::PhaseChanged(phase));
    let _ = event_tx.send(NetworkEvent::TelemetryUpdated(Box::new(snapshot)));
}

#[allow(clippy::too_many_arguments)]
async fn run_p2p_session(
    role: SessionRole,
    mut config: NetworkConfig,
    invite: Option<ParsedInvite>,
    video_rx: mpsc::Receiver<OutboundVideoFrame>,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
    input_rx: mpsc::Receiver<InputPacket>,
    control_rx: mpsc::Receiver<ControlMessage>,
    haptics_rx: watch::Receiver<crate::haptics::Rumble>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    telemetry: Arc<Mutex<NetworkTelemetry>>,
    mut shutdown_rx: watch::Receiver<bool>,
    overrides: RuntimeOverrides,
) -> Result<(), NetworkError> {
    let relay_only = role == SessionRole::Host && config.relay_only;
    if relay_only {
        config.protocol = TransportProtocol::Quic;
    }
    let identity = load_identity(config.identity_file.clone())?;
    let resume_key = (role == SessionRole::Host && config.identity_file.is_some())
        .then(|| host_resume_key(&identity));
    let saved_session = telemetry.lock().await.saved_session;
    let profile_endpoint = config
        .profile_endpoint
        .clone()
        .unwrap_or_else(|| config.signaling_url.clone());

    set_phase(ConnectionPhase::DiscoveringNat, &telemetry, &event_tx).await;
    let mut endpoint_source = if saved_session {
        "saved network"
    } else if invite
        .as_ref()
        .is_some_and(|invite| invite.endpoint_from_link)
    {
        "invite link"
    } else {
        "Signaling Endpoint setting"
    };
    if let Some(invite) = invite.as_ref().filter(|invite| invite.discover_lan) {
        config.signaling_url = tokio::select! {
            _ = shutdown_rx.wait_for(|stopped| *stopped) => return Err(NetworkError::Cancelled),
            endpoint = discover_lan_signaling(&invite.room_code) => endpoint,
        }
        .ok_or(NetworkError::LocalRoomNotFound)?;
        endpoint_source = "LAN discovery";
    }
    let public_signaling = !is_lan_signaling_url(
        config
            .advertised_signaling_url
            .as_deref()
            .unwrap_or(&config.signaling_url),
    );
    #[cfg(target_os = "android")]
    let local_bind =
        if role == SessionRole::Client && !relay_only && config.bind_addr.ip().is_unspecified() {
            url::Url::parse(&config.signaling_url)
                .ok()
                .and_then(|url| match url.host() {
                    Some(url::Host::Ipv4(peer)) => crate::signaling::android_lan_source(peer),
                    _ => None,
                })
                .map(|ip| SocketAddr::new(ip.into(), config.bind_addr.port()))
        } else {
            None
        };
    #[cfg(target_os = "android")]
    let std_socket = match local_bind.and_then(|addr| StdUdpSocket::bind(addr).ok()) {
        Some(socket) => socket,
        None => bind_media_socket(config.bind_addr, public_signaling && !relay_only)?,
    };
    #[cfg(not(target_os = "android"))]
    let std_socket = bind_media_socket(config.bind_addr, public_signaling && !relay_only)?;
    std_socket.set_nonblocking(true)?;
    let udp_socket = UdpSocket::from_std(std_socket)?;

    let candidates: IceCandidates = if relay_only {
        IceCandidates {
            local_addr: udp_socket.local_addr()?,
            lan_addrs: Vec::new(),
            public_reflexive: None,
            public_reflexive_v6: None,
        }
    } else {
        let enable_stun = config.enable_stun && public_signaling;
        tokio::select! {
            _ = shutdown_rx.wait_for(|stopped| *stopped) => return Err(NetworkError::Cancelled),
            candidates = gather_candidates(&udp_socket, &config.stun_servers, enable_stun) => candidates,
        }
    };
    let ordered_candidates = if relay_only {
        Vec::new()
    } else {
        candidates.ordered_targets()
    };

    {
        let mut guard = telemetry.lock().await;
        guard.local_udp_addr = Some(candidates.local_addr);
        guard.public_reflexive_addr = candidates
            .public_reflexive
            .or(candidates.public_reflexive_v6);
        guard.cert_fingerprint = Some(identity.fingerprint_hex.clone());
    }

    let peer_match = match role {
        SessionRole::Host => {
            set_phase(ConnectionPhase::WaitingForPeer, &telemetry, &event_tx).await;
            let (room_code, invite_uri) = if saved_session {
                let key = resume_key
                    .as_deref()
                    .ok_or_else(|| NetworkError::Permission("device identity is missing".into()))?;
                (format!("saved-{key}"), None)
            } else {
                let code = config
                    .host_room_code
                    .clone()
                    .unwrap_or_else(generate_room_code);
                let uri = build_invite_uri(
                    &code,
                    config
                        .advertised_signaling_url
                        .as_deref()
                        .unwrap_or(&config.signaling_url),
                    &identity.fingerprint_hex,
                    config.protocol == TransportProtocol::RawUdp,
                )?;
                (code, Some(uri))
            };

            let (ready_tx, ready_rx) = oneshot::channel();
            let sig_url = config.signaling_url.clone();
            let fp = identity.fingerprint_hex.clone();
            let room_clone = room_code.clone();
            let cands = ordered_candidates.clone();

            let sig_fut = signal_as_host(
                &sig_url,
                room_clone,
                fp,
                cands,
                config.protocol == TransportProtocol::RawUdp,
                config
                    .invited_client
                    .clone()
                    .map(|fingerprint| vec![fingerprint])
                    .or_else(|| config.saved_clients.clone()),
                computer_display_name(),
                Some(ready_tx),
            );
            tokio::pin!(sig_fut);

            let mut ready_rx = Some(ready_rx);
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return Err(NetworkError::Cancelled);
                        }
                    }
                    ready = async {
                        match ready_rx.as_mut() {
                            Some(rx) => rx.await.ok(),
                            None => std::future::pending().await,
                        }
                    }, if ready_rx.is_some() => {
                        ready_rx = None;
                        if let (Some(code), Some(uri)) = (ready, &invite_uri) {
                            let mut guard = telemetry.lock().await;
                            guard.room_code = Some(code.clone());
                            guard.invite_uri = Some(uri.clone());
                            let _ = event_tx.send(NetworkEvent::TelemetryUpdated(Box::new(guard.clone())));
                            let _ = event_tx.send(NetworkEvent::InviteReady {
                                room_code: code,
                                invite_uri: uri.clone(),
                            });
                        }
                    }
                    matched = &mut sig_fut => {
                        break matched.map_err(|source| NetworkError::SignalingRoute { endpoint_source, source })?;
                    }
                }
            }
        }
        SessionRole::Client => {
            set_phase(ConnectionPhase::WaitingForPeer, &telemetry, &event_tx).await;
            let parsed = invite.ok_or_else(|| {
                SignalingError::InvalidInvite("client session requires an invite".to_string())
            })?;
            if !saved_session {
                let mut guard = telemetry.lock().await;
                guard.room_code = Some(parsed.room_code.clone());
            }

            let sig_fut = async {
                let deadline = Instant::now() + Duration::from_secs(4);
                loop {
                    let result = signal_as_client(
                        &config.signaling_url,
                        parsed.room_code.clone(),
                        identity.fingerprint_hex.clone(),
                        ordered_candidates.clone(),
                    )
                    .await;
                    if !matches!(&result, Err(SignalingError::RemoteError(message))
                        if message == "host is offline or the room was already claimed")
                        || Instant::now() >= deadline
                    {
                        break result;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            };
            tokio::select! {
                _ = shutdown_rx.wait_for(|stopped| *stopped) => return Err(NetworkError::Cancelled),
                matched = sig_fut => {
                    let mut m = matched.map_err(|source| NetworkError::SignalingRoute { endpoint_source, source })?;
                    if let Some(expected_fp) = parsed.expected_host_fingerprint
                        && expected_fp.to_lowercase() != m.peer_fingerprint.to_lowercase()
                    {
                        return Err(NetworkError::Tls(
                            "signaling returned host fingerprint that does not match invite URI"
                                .to_string(),
                        ));
                    }
                    if let Some(raw_udp) = m.raw_udp {
                        if parsed.raw_udp.is_some_and(|expected| expected != raw_udp) {
                            return Err(SignalingError::InvalidInvite("room transport does not match the invite link".to_string()).into());
                        }
                        config.protocol = if raw_udp { TransportProtocol::RawUdp } else { TransportProtocol::Quic };
                    }
                    m.peer_fingerprint = m.peer_fingerprint.to_lowercase();
                    m
                }
            }
        }
    };

    let mut relay = peer_match.relay;
    let mutual_auth = relay.is_some();
    if let Some(invited) = &config.invited_client
        && (!mutual_auth || !invited.eq_ignore_ascii_case(&peer_match.peer_fingerprint))
    {
        return Err(NetworkError::Permission(
            "this device was not approved for the invite".into(),
        ));
    }
    if relay_only && !mutual_auth {
        return Err(NetworkError::RelayUnavailable);
    }
    if saved_session && !mutual_auth {
        return Err(NetworkError::Permission(
            "saved connections require authenticated QUIC and an updated signaling server".into(),
        ));
    }
    if let Some(clients) = config.saved_clients.as_ref()
        && !clients
            .iter()
            .any(|fp| fp.eq_ignore_ascii_case(&peer_match.peer_fingerprint))
    {
        return Err(NetworkError::Permission(
            "this client has no permission to reconnect".into(),
        ));
    }
    let tag = mutual_auth.then(|| {
        let mut hash = Sha256::new();
        let (host, client) = match role {
            SessionRole::Host => (&identity.fingerprint_hex, &peer_match.peer_fingerprint),
            SessionRole::Client => (&peer_match.peer_fingerprint, &identity.fingerprint_hex),
        };
        hash.update(peer_match.room_code.as_bytes());
        hash.update(host.as_bytes());
        hash.update(client.as_bytes());
        hash.finalize().into()
    });
    let candidates: Vec<_> = peer_match
        .peer_candidates
        .into_iter()
        .map(canonical_addr)
        .collect();
    let direct = if relay_only || candidates.is_empty() {
        Err(NetworkError::HolePunchTimeout)
    } else {
        set_phase(ConnectionPhase::HolePunching, &telemetry, &event_tx).await;
        // A public (WAN) punch gets the full configured budget. It used to be
        // capped at 1.5 s, which most friend-to-friend NATs cannot clear; the
        // session then fell back to the WebSocket relay and paid TCP
        // head-of-line blocking for the rest of the call. A direct path is
        // worth the extra seconds of setup, and relay-only sessions skip
        // punching entirely.
        hole_punch_udp(
            &udp_socket,
            &candidates,
            Duration::from_millis(config.hole_punch_timeout_ms),
            tag,
            shutdown_rx.clone(),
        )
        .await
    };
    if matches!(direct, Err(NetworkError::Cancelled)) {
        return Err(NetworkError::Cancelled);
    }

    if config.protocol == TransportProtocol::RawUdp {
        drop(relay);
        let active_peer_addr = direct?;
        udp_socket.connect(active_peer_addr).await?;
        discard_waiting_audio(&mut audio_rx);
        {
            let mut guard = telemetry.lock().await;
            guard.active_peer_addr = Some(canonical_addr(active_peer_addr));
            guard.connection_path = Some(ConnectionPath::direct(active_peer_addr));
        }
        set_phase(ConnectionPhase::Connected, &telemetry, &event_tx).await;
        let loop_res = run_raw_udp_loop(
            role,
            config,
            udp_socket,
            video_rx,
            audio_rx,
            input_rx,
            control_rx,
            haptics_rx,
            event_tx.clone(),
            Arc::clone(&telemetry),
            shutdown_rx,
            overrides,
        )
        .await;
        if loop_res.is_ok() {
            set_phase(ConnectionPhase::Disconnected, &telemetry, &event_tx).await;
        }
        return loop_res;
    }

    set_phase(ConnectionPhase::QuicHandshake, &telemetry, &event_tx).await;
    let quic = QuicPeer {
        role,
        identity: &identity,
        fingerprint: &peer_match.peer_fingerprint,
        mutual_auth,
        peer_candidates: &candidates,
    };
    let probe_socket = (!relay_only && !candidates.is_empty() && relay.is_some())
        .then(|| {
            socket2::SockRef::from(&udp_socket)
                .try_clone()
                .ok()
                .and_then(|socket| UdpSocket::from_std(socket.into()).ok())
        })
        .flatten();
    let (probe_key, probe_key_rx) = watch::channel(None);
    let (direct_path, direct_path_rx) = watch::channel(None);
    let mut attempt = None;
    if let Some(signaling) = relay.as_mut() {
        if agree_direct(signaling, direct.is_ok(), &mut shutdown_rx).await? {
            let result = quic
                .connect(
                    udp_socket,
                    direct?,
                    Duration::from_millis(2500),
                    &mut shutdown_rx,
                )
                .await;
            if matches!(result, Err(NetworkError::Cancelled)) {
                return Err(NetworkError::Cancelled);
            }
            if agree_direct(signaling, result.is_ok(), &mut shutdown_rx).await? {
                attempt = Some(result?);
            }
        }
    } else {
        attempt = Some(
            quic.connect(
                udp_socket,
                direct?,
                Duration::from_secs(8),
                &mut shutdown_rx,
            )
            .await?,
        );
    }

    let (endpoint, connection, _relay_task) = if let Some((endpoint, connection)) = attempt {
        (endpoint, connection, None)
    } else {
        let signaling = relay.take().ok_or(NetworkError::HolePunchTimeout)?;
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let bridge = UdpSocket::bind("127.0.0.1:0").await?;
        let bridge_addr = bridge.local_addr()?;
        bridge.connect(socket.local_addr()?).await?;
        let relay_path_rx = direct_path_rx.clone();
        let probe_candidates = candidates.clone();
        let task = RelayTask(tokio::spawn(async move {
            let result = if let Some(udp) = probe_socket {
                let bridge = Arc::new(bridge);
                let udp = Arc::new(udp);
                let upgrade = crate::relay_upgrade::RelayUpgrade {
                    udp: udp.clone(),
                    candidates: probe_candidates,
                    host: role == SessionRole::Host,
                    key: probe_key_rx,
                    path: direct_path.clone(),
                };
                let direct_receive = async {
                    let result = upgrade.receive(bridge.clone()).await;
                    direct_path.send_replace(None);
                    if let Err(error) = result {
                        debug!(%error, "direct path probe stopped");
                    }
                    std::future::pending::<()>().await;
                };
                tokio::select! {
                    _ = direct_receive => unreachable!(),
                    result = signaling.forward_routes(&bridge, Some((&udp, relay_path_rx))) => result,
                }
            } else {
                signaling.forward(bridge).await
            };
            if let Err(error) = result {
                debug!(%error, "relay stopped");
            }
        }));
        let (endpoint, connection) = quic
            .connect(
                socket,
                bridge_addr,
                Duration::from_secs(8),
                &mut shutdown_rx,
            )
            .await?;
        telemetry.lock().await.relayed = true;
        (endpoint, connection, Some(task))
    };
    drop(relay);
    if _relay_task.is_some() {
        let mut key = [0; 32];
        if connection
            .export_keying_material(&mut key, b"EXPORTER-VmaX-direct-path-v1", b"")
            .is_ok()
        {
            probe_key.send_replace(Some(key));
        }
    }
    discard_waiting_audio(&mut audio_rx);
    let startup_bitrate = quic_startup_bitrate(
        config.initial_bitrate_bps,
        _relay_task.is_some(),
        connection.rtt(),
    );
    if role == SessionRole::Host
        && !config.fixed_bitrate
        && startup_bitrate < config.initial_bitrate_bps
    {
        config.initial_bitrate_bps = startup_bitrate;
        telemetry.lock().await.target_bitrate_bps = config.initial_bitrate_bps;
        let _ = event_tx.send(NetworkEvent::BitrateUpdated(BitrateDecision {
            target_bitrate_bps: config.initial_bitrate_bps,
            state: CongestionState::Stable,
            rtt_ms: connection.rtt().as_secs_f64() * 1000.0,
            min_rtt_ms: connection.rtt().as_secs_f64() * 1000.0,
            loss_pct: 0.0,
            reason: if _relay_task.is_some() {
                "relay startup".into()
            } else {
                "high RTT startup".into()
            },
            encoder_reconfigure_recommended: true,
        }));
    }
    info!(
        ?role,
        relayed = _relay_task.is_some(),
        "QUIC connection established"
    );
    {
        let mut guard = telemetry.lock().await;
        guard.active_peer_addr = Some(canonical_addr(connection.remote_address()));
        guard.connection_path = Some(if _relay_task.is_some() {
            ConnectionPath::Relay
        } else {
            ConnectionPath::direct(connection.remote_address())
        });
        if mutual_auth
            && config.identity_file.is_some()
            && is_fingerprint(&peer_match.peer_fingerprint)
        {
            guard.trusted_peer = Some(TrustedPeer {
                fingerprint: peer_match.peer_fingerprint.to_ascii_lowercase(),
                endpoint: profile_endpoint,
                resume_key: None,
            });
            guard.peer_savable = role == SessionRole::Host;
        }
    }
    set_phase(ConnectionPhase::Connected, &telemetry, &event_tx).await;

    let mut shutdown_watch = shutdown_rx.clone();
    let shutdown_connection = connection.clone();
    let _close_on_shutdown = RelayTask(tokio::spawn(async move {
        if shutdown_watch.wait_for(|stopped| *stopped).await.is_ok() {
            shutdown_connection.close(0u32.into(), b"local shutdown");
        }
    }));

    #[cfg(target_os = "android")]
    let _mobile_rebind = (role == SessionRole::Client && _relay_task.is_none() && public_signaling)
        .then(|| {
            let endpoint = endpoint.clone();
            let conn = connection.clone();
            let mut epoch = crate::nat_mobility::network_epoch();
            RelayTask(tokio::spawn(async move {
                let mut check = interval(Duration::from_millis(200));
                let mut watchdog = crate::nat_mobility::MigrationWatchdog::new(Instant::now());
                loop {
                    tokio::select! {
                        _ = conn.closed() => break,
                        _ = check.tick() => {
                            let next = crate::nat_mobility::network_epoch();
                            let epoch_changed = next != epoch;
                            // The OS callback is the primary trigger. The
                            // watchdog catches missed callbacks and silently
                            // expired CGNAT mappings: QUIC keeps transmitting
                            // into a dead path, so only received datagrams
                            // prove it is still alive.
                            let stalled = matches!(
                                watchdog.on_tick(&conn, Instant::now()),
                                crate::nat_mobility::MigrationHint::RebindSuggested
                            );
                            if !epoch_changed && !stalled {
                                continue;
                            }
                            let Ok(local) = endpoint.local_addr() else { break; };
                            let bind = if local.is_ipv6() { "[::]:0".parse() } else { "0.0.0.0:0".parse() };
                            let Ok(bind) = bind else { continue; };
                            let before = crate::nat_mobility::path_snapshot(&conn);
                            match crate::nat_mobility::rebind_endpoint(&endpoint, bind) {
                                Ok(()) => {
                                    epoch = next;
                                    watchdog.note_rebind(Instant::now());
                                    debug!(
                                        rtt_ms = before.rtt.as_millis() as u64,
                                        min_rtt_ms = before.min_rtt.as_millis() as u64,
                                        loss_permille = before.loss_permille,
                                        stalled,
                                        "mobile QUIC path rebound",
                                    );
                                }
                                Err(error) => debug!(%error, "mobile QUIC rebind deferred"),
                            }
                        }
                    }
                }
            }))
        });

    let loop_res = run_quic_multiplex_loop(
        role,
        config,
        connection.clone(),
        video_rx,
        audio_rx,
        input_rx,
        control_rx,
        haptics_rx,
        event_tx.clone(),
        Arc::clone(&telemetry),
        shutdown_rx.clone(),
        overrides,
        if mutual_auth { resume_key } else { None },
        _relay_task.is_none(),
        direct_path_rx,
    )
    .await;

    connection.close(0u32.into(), b"session ended");
    let _ = timeout(Duration::from_millis(250), endpoint.wait_idle()).await;
    if *shutdown_rx.borrow() || loop_res.is_ok() {
        set_phase(ConnectionPhase::Disconnected, &telemetry, &event_tx).await;
        Ok(())
    } else {
        loop_res
    }
}

fn discard_waiting_audio(audio: &mut mpsc::Receiver<AudioPacket>) {
    for _ in 0..audio.len() {
        let _ = audio.try_recv();
    }
}

async fn agree_direct(
    relay: &mut RelayConnection,
    ready: bool,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<bool, NetworkError> {
    tokio::select! {
        _ = shutdown.wait_for(|stopped| *stopped) => Err(NetworkError::Cancelled),
        result = relay.agree_direct(ready) => Ok(result?),
    }
}

struct RelayTask(tokio::task::JoinHandle<()>);

impl Drop for RelayTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct QuicPeer<'a> {
    role: SessionRole,
    identity: &'a EphemeralIdentity,
    fingerprint: &'a str,
    mutual_auth: bool,
    peer_candidates: &'a [SocketAddr],
}

impl QuicPeer<'_> {
    async fn connect(
        &self,
        socket: UdpSocket,
        peer_addr: SocketAddr,
        handshake_timeout: Duration,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(Endpoint, Connection), NetworkError> {
        let socket = socket.into_std()?;
        let server_config = if self.role == SessionRole::Host {
            Some(build_server_config(
                self.identity,
                self.mutual_auth.then_some(self.fingerprint),
            )?)
        } else {
            None
        };
        let mut endpoint_config = EndpointConfig::default();
        endpoint_config
            .max_udp_payload_size(crate::nat_mobility::CELLULAR_MAX_MTU)
            .map_err(io::Error::other)?;
        let mut endpoint = Endpoint::new(
            endpoint_config,
            server_config,
            socket,
            Arc::new(TokioRuntime),
        )?;

        let handshake = async {
            match self.role {
                SessionRole::Host => {
                    while let Some(incoming) = endpoint.accept().await {
                        let remote = incoming.remote_address();
                        let known_interface = self.mutual_auth
                            && self
                                .peer_candidates
                                .iter()
                                .any(|candidate| candidate.ip() == canonical_addr(remote).ip());
                        if canonical_addr(remote) != canonical_addr(peer_addr) && !known_interface {
                            incoming.refuse();
                            continue;
                        }
                        match incoming.await {
                            Ok(conn) => return Ok(conn),
                            Err(error) => debug!(%error, "incoming QUIC handshake failed"),
                        }
                    }
                    Err(NetworkError::Cancelled)
                }
                SessionRole::Client => {
                    let cfg = build_client_config(
                        self.fingerprint,
                        self.mutual_auth.then_some(self.identity),
                    )?;
                    endpoint.set_default_client_config(cfg);
                    Ok(endpoint.connect(peer_addr, "vmax.peer")?.await?)
                }
            }
        };
        let connection = tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return Err(NetworkError::Cancelled),
            result = timeout(handshake_timeout, handshake) => {
                result.map_err(|_| NetworkError::HandshakeTimeout)??
            }
        };
        Ok((endpoint, connection))
    }
}

struct SessionCounters {
    media_bitrate: std::sync::Mutex<crate::telemetry::BitrateCounter>,
    video_frames_tx: AtomicU64,
    video_frames_rx: AtomicU64,
    video_frames_dropped: AtomicU64,
    fec_recovered_shards: AtomicU64,
    audio_packets_tx: AtomicU64,
    audio_packets_rx: AtomicU64,
    input_events_tx: AtomicU64,
    input_events_rx: AtomicU64,
    peer_frames_dropped: AtomicU64,
}

impl Default for SessionCounters {
    fn default() -> Self {
        Self {
            media_bitrate: std::sync::Mutex::new(crate::telemetry::BitrateCounter::new(
                Duration::from_secs(1),
            )),
            video_frames_tx: AtomicU64::new(0),
            video_frames_rx: AtomicU64::new(0),
            video_frames_dropped: AtomicU64::new(0),
            fec_recovered_shards: AtomicU64::new(0),
            audio_packets_tx: AtomicU64::new(0),
            audio_packets_rx: AtomicU64::new(0),
            input_events_tx: AtomicU64::new(0),
            input_events_rx: AtomicU64::new(0),
            peer_frames_dropped: AtomicU64::new(0),
        }
    }
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn handle_inbound_media_datagram(
    dgram: &[u8],
    reassembler: &mut VideoReassembler,
    last_pli: &mut Instant,
    counters: &SessionCounters,
    event_tx: &mpsc::UnboundedSender<NetworkEvent>,
    ctrl_tx: &mpsc::Sender<ControlMessage>,
    mut feedback: Option<&mut crate::congestion::FeedbackBuilder>,
    feedback_sink: Option<&mpsc::UnboundedSender<crate::congestion::FeedbackPacket>>,
    accept_rumble: bool,
) {
    if dgram.is_empty() {
        return;
    }

    if matches!(
        ChannelId::try_from(dgram[0]),
        Ok(ChannelId::Video | ChannelId::Audio)
    ) && let Ok(mut bitrate) = counters.media_bitrate.lock()
    {
        bitrate.add(dgram.len(), Instant::now());
    }

    // Sample data and unweighted parity before reassembly so the host can
    // match arrivals even when FEC is still recovering the frame.
    if dgram[0] == ChannelId::Video as u8
        && let Some(builder) = feedback.as_deref_mut()
        && let Ok((header, _)) = crate::protocol::VideoChunkHeader::decode(dgram)
        && header.flags & crate::protocol::video_flags::WEIGHTED_PARITY == 0
    {
        builder.on_video_datagram(
            header.frame_id as u32,
            header.chunk_idx,
            header.timestamp_us,
            Instant::now(),
            dgram.len(),
            header.is_parity(),
        );
    }

    match ChannelId::try_from(dgram[0]) {
        Ok(ChannelId::Video) => match reassembler.push_datagram(dgram) {
            Ok(ReassembleOutcome::Completed {
                frame,
                dropped_frames,
            }) => {
                counters
                    .video_frames_dropped
                    .fetch_add(u64::from(dropped_frames), Ordering::Relaxed);
                counters.video_frames_rx.fetch_add(1, Ordering::Relaxed);
                if frame.fec_recovered_shards > 0 {
                    counters
                        .fec_recovered_shards
                        .fetch_add(frame.fec_recovered_shards as u64, Ordering::Relaxed);
                }
                if let Some(builder) = feedback.as_deref_mut() {
                    builder.note_completed_video(frame.frame_id, frame.is_keyframe);
                    if !frame.is_keyframe && dropped_frames > 0 {
                        builder.note_rfi();
                    }
                }
                let _ = event_tx.send(NetworkEvent::VideoFrameReceived(frame));
            }
            Ok(ReassembleOutcome::DroppedNeedKeyframe { dropped_frames }) => {
                counters
                    .video_frames_dropped
                    .fetch_add(dropped_frames as u64, Ordering::Relaxed);
                if let Some(builder) = feedback.as_deref_mut() {
                    builder.note_rfi();
                }
                let fallback_interval = if feedback.is_some() { 600 } else { 250 };
                if last_pli.elapsed() >= Duration::from_millis(fallback_interval) {
                    *last_pli = Instant::now();
                    let _ = ctrl_tx.try_send(ControlMessage::RequestKeyframe {
                        last_good_frame_id: None,
                        reason: format!("dropped {dropped_frames} frame(s) beyond jitter window"),
                    });
                }
            }
            Ok(ReassembleOutcome::Incomplete) => {}
            Err(e) => debug!(error = %e, "malformed video datagram ignored"),
        },
        Ok(ChannelId::Audio) => match AudioPacket::decode(dgram) {
            Ok(pkt) => {
                counters.audio_packets_rx.fetch_add(1, Ordering::Relaxed);
                let _ = event_tx.send(NetworkEvent::AudioPacketReceived(pkt));
            }
            Err(e) => debug!(error = %e, "malformed audio datagram ignored"),
        },
        Ok(ChannelId::Input) => match InputPacket::decode(dgram) {
            Ok(pkt) => {
                counters.input_events_rx.fetch_add(1, Ordering::Relaxed);
                let _ = event_tx.send(NetworkEvent::InputEventReceived(pkt));
            }
            Err(e) => debug!(error = %e, "malformed input datagram ignored"),
        },
        Ok(ChannelId::Feedback) => {
            if let Some(packet) = crate::congestion::FeedbackPacket::decode_datagram(dgram) {
                if let Some(sink) = feedback_sink {
                    let _ = sink.send(packet);
                }
                let _ = event_tx.send(NetworkEvent::CongestionFeedback(Box::new(packet)));
            }
        }
        Ok(ChannelId::Haptics) if accept_rumble => {
            if let Some(rumble) = crate::haptics::Rumble::decode(dgram) {
                let _ = event_tx.send(NetworkEvent::RumbleReceived(rumble));
            }
        }
        Ok(ChannelId::Haptics | ChannelId::Control) | Err(_) => {}
    }
}

async fn publish_telemetry_snapshot(
    decision: &BitrateDecision,
    counters: &SessionCounters,
    telemetry: &Arc<Mutex<NetworkTelemetry>>,
    event_tx: &mpsc::UnboundedSender<NetworkEvent>,
) {
    let snapshot = {
        let mut guard = telemetry.lock().await;
        guard.rtt_ms = decision.rtt_ms;
        guard.min_rtt_ms = decision.min_rtt_ms;
        guard.packet_loss_pct = decision.loss_pct;
        guard.target_bitrate_bps = decision.target_bitrate_bps;
        guard.congestion_state = decision.state;
        guard.video_frames_tx = counters.video_frames_tx.load(Ordering::Relaxed);
        guard.video_frames_rx = counters.video_frames_rx.load(Ordering::Relaxed);
        guard.video_frames_dropped = counters.video_frames_dropped.load(Ordering::Relaxed);
        guard.fec_recovered_shards = counters.fec_recovered_shards.load(Ordering::Relaxed);
        guard.audio_packets_tx = counters.audio_packets_tx.load(Ordering::Relaxed);
        guard.audio_packets_rx = counters.audio_packets_rx.load(Ordering::Relaxed);
        if let Ok(mut bitrate) = counters.media_bitrate.lock() {
            bitrate.add(0, Instant::now());
            guard.media_bitrate_mbps = bitrate.mbps();
        }
        guard.input_events_tx = counters.input_events_tx.load(Ordering::Relaxed);
        guard.input_events_rx = counters.input_events_rx.load(Ordering::Relaxed);
        guard.clone()
    };
    let _ = event_tx.send(NetworkEvent::TelemetryUpdated(Box::new(snapshot)));
}

fn sync_bitrate_decision(
    bitrate_ctrl: &mut BitrateController,
    last_max_bps: &mut u32,
    live_max_bps: u32,
    sample: PathTelemetrySample,
) -> BitrateDecision {
    let changed = live_max_bps != *last_max_bps && live_max_bps > 0;
    if changed {
        *last_max_bps = live_max_bps;
        bitrate_ctrl.set_max_bitrate_bps(live_max_bps);
    }
    let mut decision = bitrate_ctrl.update(sample);
    if changed {
        decision.encoder_reconfigure_recommended = true;
    }
    decision
}

fn video_reassembly_windows(
    base: Duration,
    rtt: Duration,
    variation: Duration,
) -> (Duration, Duration) {
    let allowance = (variation * 2).min(Duration::from_millis(60));
    let delta = (base.max(rtt / 2) + allowance).min(Duration::from_millis(140));
    // A keyframe can be many times the size of a delta frame.  The pacer may
    // legitimately need 50-100 ms to drain one at a low startup bitrate, so
    // the keyframe window must outlive that spread.  This only delays giving
    // up on an incomplete keyframe; a complete one is emitted immediately.
    let keyframe = (base.max(rtt * 2) + allowance)
        .max(Duration::from_millis(120))
        .min(Duration::from_millis(500));
    (delta, keyframe)
}

#[cfg(test)]
#[test]
fn fixed_bitrate_uses_the_host_target_and_honors_the_viewer_limit() {
    let overrides = RuntimeOverrides {
        fec_group_size: Arc::new(AtomicU8::new(0)),
        bitrate_policy: Arc::new(AtomicU32::new((200_000_000 << 1) | 1)),
        client_limit_bps: Arc::new(AtomicU32::new(0)),
        backlog: VideoBacklog::new(),
    };
    assert_eq!(overrides.target_bitrate(4_000_000), 200_000_000);
    overrides
        .client_limit_bps
        .store(35_000_000, Ordering::Relaxed);
    assert_eq!(overrides.target_bitrate(4_000_000), 35_000_000);
    overrides
        .bitrate_policy
        .store(80_000_000 << 1, Ordering::Relaxed);
    assert_eq!(overrides.target_bitrate(16_000_000), 16_000_000);
    assert_eq!(overrides.target_bitrate(40_000_000), 35_000_000);
}

#[cfg(test)]
#[test]
fn backlog_reports_saturation_only_against_a_known_cap() {
    let backlog = VideoBacklog::new();
    // No cap published yet: the sender has not decided what fits, so nothing
    // may be treated as over budget.
    assert!(!backlog.saturated());
    backlog.set_cap_bytes(64 * 1024);
    assert_eq!(backlog.cap_bytes(), 64 * 1024);
    assert!(!backlog.saturated());
    // The QUIC side alone can fill the budget while the pacer is empty; the
    // capture loop must still see the queue as full.
    backlog.note_quic_bytes(64 * 1024);
    assert_eq!(backlog.queued_bytes(), 64 * 1024);
    assert!(backlog.saturated());
    backlog.note_quic_bytes(0);
    assert!(!backlog.saturated());
}

#[cfg(test)]
#[test]
fn jitter_extends_recovery_without_changing_the_stable_path_window() {
    let ms = Duration::from_millis;
    assert_eq!(
        video_reassembly_windows(ms(18), ms(8), ms(0)),
        (ms(18), ms(120))
    );
    assert_eq!(
        video_reassembly_windows(ms(18), ms(64), ms(0)),
        (ms(32), ms(128))
    );
    assert_eq!(
        video_reassembly_windows(ms(18), ms(64), ms(14)),
        (ms(60), ms(156))
    );
    assert_eq!(
        video_reassembly_windows(ms(18), ms(800), ms(150)),
        (ms(140), ms(500))
    );
    assert_eq!(quic_startup_bitrate(20_000_000, false, ms(8)), 20_000_000);
    assert_eq!(quic_startup_bitrate(20_000_000, false, ms(64)), 6_000_000);
    assert_eq!(quic_startup_bitrate(20_000_000, true, ms(8)), 4_000_000);
    assert_eq!(quic_startup_bitrate(2_500_000, false, ms(64)), 2_500_000);
    assert_eq!(
        pending_video_cap(4_000_000, 60, ms(100), false, Duration::ZERO),
        16_666
    );
    assert_eq!(
        pending_video_cap(4_000_000, 60, ms(100), true, Duration::ZERO),
        65_536
    );
}

#[allow(clippy::too_many_arguments)]
async fn run_quic_multiplex_loop(
    role: SessionRole,
    config: NetworkConfig,
    conn: Connection,
    mut video_rx: mpsc::Receiver<OutboundVideoFrame>,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
    mut input_rx: mpsc::Receiver<InputPacket>,
    mut control_rx: mpsc::Receiver<ControlMessage>,
    mut haptics_rx: watch::Receiver<crate::haptics::Rumble>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    telemetry: Arc<Mutex<NetworkTelemetry>>,
    mut shutdown_rx: watch::Receiver<bool>,
    overrides: RuntimeOverrides,
    resume_key: Option<String>,
    originally_direct: bool,
    mut direct_path: watch::Receiver<Option<SocketAddr>>,
) -> Result<(), NetworkError> {
    let counters = Arc::new(SessionCounters::default());

    let mut ctrl_stream = conn.open_uni().await?;
    let _ = ctrl_stream.set_priority(100);
    ctrl_stream
        .write_all(&[StreamKind::ControlReliable as u8])
        .await
        .map_err(|e| NetworkError::Io(io::Error::other(e)))?;

    let mut input_stream = conn.open_uni().await?;
    let _ = input_stream.set_priority(90);
    input_stream
        .write_all(&[StreamKind::InputReliable as u8])
        .await
        .map_err(|e| NetworkError::Io(io::Error::other(e)))?;

    let hello = ControlMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        role: format!("{role:?}"),
        max_bitrate_bps: config.max_bitrate_bps,
        target_fps: config.target_fps,
        resume_key,
        two_shard_fec: true,
    };
    let hello_bytes = hello.encode_framed()?;
    ctrl_stream
        .write_all(&hello_bytes)
        .await
        .map_err(|e| NetworkError::Io(io::Error::other(e)))?;

    let (internal_ctrl_tx, mut internal_ctrl_rx) = mpsc::channel::<ControlMessage>(64);
    let (peer_closed_tx, mut peer_closed_rx) = watch::channel(false);

    let (feedback_tx, mut feedback_rx) =
        mpsc::unbounded_channel::<crate::congestion::FeedbackPacket>();
    let dgram_conn = conn.clone();
    let dgram_events = event_tx.clone();
    let dgram_counters = Arc::clone(&counters);
    let dgram_ctrl_tx = internal_ctrl_tx.clone();
    let dgram_feedback_tx = feedback_tx.clone();
    // A WebSocket relay can batch datagrams during short scheduler or TCP
    // stalls even on a low-RTT route. Give relayed frames enough time to
    // reassemble without delaying delivery of complete frames.
    let minimum_window_ms = if originally_direct { 0 } else { 35 };
    let jitter_window = Duration::from_millis(config.jitter_window_ms.max(minimum_window_ms));
    let recovery_allowance = jitter_window
        .saturating_sub(Duration::from_millis(18))
        .min(Duration::from_millis(35));
    let dgram_task = tokio::spawn(async move {
        let mut reassembler = VideoReassembler::new(jitter_window);
        let mut rtt = dgram_conn.rtt();
        let mut variation = Duration::ZERO;
        let (delta, keyframe) = video_reassembly_windows(jitter_window, rtt, variation);
        reassembler.set_jitter_windows(delta, keyframe);
        let mut next_rtt_sample = Instant::now() + Duration::from_millis(100);
        let mut last_pli = Instant::now() - Duration::from_secs(1);
        // Only the receiving client samples arrival times.  The host emits no
        // feedback reports of its own.
        let mut feedback_builder = (role == SessionRole::Client)
            .then(|| crate::congestion::FeedbackBuilder::new(Instant::now()));
        let mut feedback_tick = tokio::time::interval_at(
            (Instant::now() + crate::congestion::FEEDBACK_INTERVAL).into(),
            crate::congestion::FEEDBACK_INTERVAL,
        );
        feedback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                dgram = dgram_conn.read_datagram() => {
                    let Ok(dgram) = dgram else {
                        break;
                    };
                    let now = Instant::now();
                    if now >= next_rtt_sample {
                        let measured_rtt = dgram_conn.rtt();
                        variation = variation * 3 / 4 + rtt.abs_diff(measured_rtt) / 4;
                        rtt = measured_rtt;
                        let (delta, keyframe) = video_reassembly_windows(jitter_window, rtt, variation);
                        reassembler.set_jitter_windows(delta, keyframe);
                        next_rtt_sample = now + Duration::from_millis(100);
                    }
                    handle_inbound_media_datagram(
                        &dgram,
                        &mut reassembler,
                        &mut last_pli,
                        &dgram_counters,
                        &dgram_events,
                        &dgram_ctrl_tx,
                        feedback_builder.as_mut(),
                        Some(&dgram_feedback_tx),
                        role == SessionRole::Client,
                    );
                }
                _ = feedback_tick.tick() => {
                    if let Some(builder) = feedback_builder.as_mut() {
                        let now = Instant::now();
                        builder.set_rtt(dgram_conn.rtt());
                        if let Some(packet) = builder.build(now) {
                            // Feedback is advisory; a full QUIC datagram
                            // buffer must never stall video delivery.
                            let _ = dgram_conn.send_datagram(packet.encode_datagram());
                        }
                    }
                }
            }
        }
    });

    let uni_conn = conn.clone();
    let uni_events = event_tx.clone();
    let uni_counters = Arc::clone(&counters);
    let uni_ctrl_tx = internal_ctrl_tx.clone();
    let uni_closed_tx = peer_closed_tx;
    let uni_telemetry = Arc::clone(&telemetry);
    let peer_two_shard_fec = Arc::new(AtomicBool::new(false));
    let uni_two_shard_fec = Arc::clone(&peer_two_shard_fec);
    let uni_task = tokio::spawn(async move {
        while let Ok(mut recv) = uni_conn.accept_uni().await {
            let events = uni_events.clone();
            let ctrs = Arc::clone(&uni_counters);
            let ctrl_reply = uni_ctrl_tx.clone();
            let closed_tx = uni_closed_tx.clone();
            let telemetry = Arc::clone(&uni_telemetry);
            let two_shard_fec = Arc::clone(&uni_two_shard_fec);

            tokio::spawn(async move {
                let mut kind_byte = [0u8; 1];
                if recv.read_exact(&mut kind_byte).await.is_err() {
                    return;
                }
                let Ok(kind) = StreamKind::try_from(kind_byte[0]) else {
                    return;
                };

                loop {
                    let mut len_buf = [0u8; 4];
                    if recv.read_exact(&mut len_buf).await.is_err() {
                        break;
                    }
                    let msg_len = u32::from_be_bytes(len_buf) as usize;
                    if msg_len == 0 || msg_len > 1024 * 1024 {
                        break;
                    }
                    let mut payload = vec![0u8; msg_len];
                    if recv.read_exact(&mut payload).await.is_err() {
                        break;
                    }

                    match kind {
                        StreamKind::InputReliable => {
                            if let Ok(pkt) = InputPacket::decode(&payload) {
                                ctrs.input_events_rx.fetch_add(1, Ordering::Relaxed);
                                let _ = events.send(NetworkEvent::InputEventReceived(pkt));
                            }
                        }
                        StreamKind::ControlReliable => {
                            if let Ok(msg) = ControlMessage::decode_json(&payload) {
                                if let ControlMessage::Hello {
                                    two_shard_fec: true,
                                    ..
                                } = &msg
                                {
                                    two_shard_fec.store(true, Ordering::Relaxed);
                                }
                                let should_close =
                                    matches!(&msg, ControlMessage::Disconnect { .. });
                                match &msg {
                                    ControlMessage::Hello {
                                        resume_key: Some(key),
                                        ..
                                    } if role == SessionRole::Client && is_fingerprint(key) => {
                                        let mut snapshot = telemetry.lock().await;
                                        if snapshot.phase == ConnectionPhase::Connected
                                            && let Some(peer) = snapshot.trusted_peer.as_mut()
                                        {
                                            peer.resume_key = Some(key.to_ascii_lowercase());
                                            snapshot.peer_savable = true;
                                            let _ = events.send(NetworkEvent::TelemetryUpdated(
                                                Box::new(snapshot.clone()),
                                            ));
                                        }
                                        continue;
                                    }
                                    ControlMessage::Ping { client_ts_us } => {
                                        let _ = ctrl_reply.try_send(ControlMessage::Pong {
                                            client_ts_us: *client_ts_us,
                                            peer_ts_us: now_unix_us(),
                                        });
                                    }
                                    ControlMessage::RequestKeyframe {
                                        last_good_frame_id,
                                        reason,
                                    } => {
                                        let _ = events.send(NetworkEvent::KeyframeRequested {
                                            last_good_frame_id: *last_good_frame_id,
                                            reason: reason.clone(),
                                        });
                                    }
                                    ControlMessage::TelemetryFeedback {
                                        frames_dropped, ..
                                    } => {
                                        ctrs.peer_frames_dropped
                                            .store(*frames_dropped, Ordering::Relaxed);
                                    }
                                    _ => {}
                                }
                                let _ = events.send(NetworkEvent::ControlReceived(msg));
                                if should_close {
                                    let _ = closed_tx.send(true);
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    let mut bitrate_ctrl =
        BitrateController::new(config.initial_bitrate_bps, config.max_bitrate_bps);
    let mut last_max_bitrate_bps = config.max_bitrate_bps;
    let mut reported_target = config.initial_bitrate_bps;
    let mut gcc = crate::congestion::GccController::with_sensitivity(
        config.initial_bitrate_bps,
        config.max_bitrate_bps,
        match config.congestion_sensitivity {
            CongestionSensitivity::Balanced => crate::congestion::GccSensitivity::Balanced,
            CongestionSensitivity::LowLatency => crate::congestion::GccSensitivity::LowLatency,
            CongestionSensitivity::Quality => crate::congestion::GccSensitivity::Quality,
        },
    );
    let mut adaptive_fec = crate::transport::AdaptiveFec::with_group_size(config.fec_group_size);
    let mut recovery =
        crate::encoder_recovery::RecoveryController::new(crate::encoder_recovery::RecoveryConfig {
            // LTR is not exposed by the current NVENC wrapper; fall back to
            // intra-refresh waves instead of referencing a stale slot.
            enable_ltr: false,
            ..Default::default()
        });
    let mut last_feedback: Option<Instant> = None;
    let mut gcc_decision: Option<BitrateDecision> = None;
    let mut live_fec_group = config.fec_group_size;
    let pacer = crate::transport::PacedSender::spawn_with_stats(
        conn.clone(),
        crate::transport::paced_wire_rate(
            config.initial_bitrate_bps,
            adaptive_fec.current().parity_permille(),
        ),
        3,
        overrides.backlog.pacer_stats(),
    );
    let mut last_pacer_errors = 0u64;
    let mut last_path_packets = (0u64, 0u64);
    let mut path_loss_permille = 0u16;
    let mut telemetry_tick = interval(Duration::from_millis(200));
    let mut last_video_frame: Option<u32> = None;
    let mut video_recovery = VideoRecovery::default();
    let mut last_keyframe_sent = None::<Instant>;
    let mut direct = originally_direct;

    let res = loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break Ok(());
                }
            }
            reason = conn.closed() => {
                match reason {
                    quinn::ConnectionError::LocallyClosed => break Ok(()),
                    quinn::ConnectionError::ApplicationClosed(ref close)
                        if close.error_code == 0u32.into() => break Ok(()),
                    reason => break Err(NetworkError::QuicConnection(reason)),
                }
            }
            Ok(()) = peer_closed_rx.changed() => {
                if *peer_closed_rx.borrow() {
                    break Ok(());
                }
            }
            Ok(()) = direct_path.changed(), if !originally_direct => {
                let peer = *direct_path.borrow_and_update();
                direct = peer.is_some();
                let mut stats = telemetry.lock().await;
                stats.relayed = !direct;
                stats.connection_path = Some(peer.map_or(ConnectionPath::Relay, ConnectionPath::direct));
                stats.active_peer_addr = Some(canonical_addr(peer.unwrap_or_else(|| conn.remote_address())));
            }
            Some(frame) = video_rx.recv() => {
                let queued = QUIC_DATAGRAM_BUFFER_BYTES.saturating_sub(conn.datagram_send_buffer_space())
                    .saturating_add(pacer.stats().queue_bytes as usize);
                let adaptive_bps = if last_feedback.is_some_and(|stamp| stamp.elapsed() < Duration::from_millis(600)) {
                    gcc.current_bitrate_bps()
                } else {
                    bitrate_ctrl.current_bitrate_bps()
                };
                let target_bitrate = overrides.target_bitrate(adaptive_bps);
                let configured_fec = overrides.fec_group_size.load(Ordering::Relaxed);
                if live_fec_group != configured_fec {
                    live_fec_group = configured_fec;
                    adaptive_fec = crate::transport::AdaptiveFec::with_group_size(live_fec_group);
                }
                pacer.set_rate_bps(crate::transport::paced_wire_rate(
                    target_bitrate, adaptive_fec.current().parity_permille(),
                ));
                let dynamic_cap = if keyframe_is_draining(last_keyframe_sent) {
                    MAX_QUEUED_DELTA_BYTES
                } else {
                    pending_video_cap(target_bitrate as usize, frame.fps, conn.rtt(), direct, recovery_allowance)
                };
                // Publish the queue before deciding anything: the capture loop
                // uses it to stop producing frames that would be dropped here.
                overrides
                    .backlog
                    .note_quic_bytes(QUIC_DATAGRAM_BUFFER_BYTES.saturating_sub(conn.datagram_send_buffer_space()) as u64);
                overrides.backlog.set_cap_bytes(dynamic_cap);
                let gap = last_video_frame.is_some_and(|id| id.wrapping_add(1) != frame.frame_id);
                let delta_send_deadline = max_capture_queue_age(frame.fps);
                let max_frame_age = if frame.is_keyframe {
                    Duration::from_millis(500)
                } else {
                    delta_send_deadline
                };
                // Drop captures that already sat too long before the pacer.
                // A fresh keyframe can still take longer to serialize.
                let stale = frame.captured_at.elapsed() > delta_send_deadline;
                if stale || !frame.is_keyframe && (video_recovery.awaiting_keyframe || gap || queued >= dynamic_cap) {
                    video_recovery.request_keyframe(&event_tx, "video queue lost a reference frame");
                    continue;
                }
                let max_dgram = crate::transport::safe_datagram_size(conn.max_datagram_size());
                let fec_decision = adaptive_fec.current();
                let two_shard_fec = direct
                    && peer_two_shard_fec.load(Ordering::Relaxed)
                    && fec_decision.two_shard;
                // `AdaptiveFec` already accounts for the second parity shard
                // in its group size.  If the peer cannot consume two shards,
                // halve the group so the intended parity percentage survives.
                let fec_group = if fec_decision.two_shard && !two_shard_fec {
                    (fec_decision.group_size / 2).max(1)
                } else {
                    fec_decision.group_size
                };
                let packets = match packetize_video(
                    frame.codec,
                    u64::from(frame.frame_id),
                    frame.timestamp_us,
                    frame.is_keyframe,
                    &frame.payload,
                    max_dgram,
                    fec_group,
                    two_shard_fec,
                    frame.meta,
                ) {
                    Ok(packets) => packets,
                    Err(error) => {
                        tracing::warn!(%error, frame_id = frame.frame_id, "Dropping unpacketizable video frame");
                        video_recovery.request_keyframe(&event_tx, "video frame exceeded transport limits");
                        continue;
                    }
                };
                let frame_bytes = packets.iter().map(Bytes::len).sum::<usize>();
                let mut shards = Vec::with_capacity(packets.len());
                for packet in packets {
                    if let Ok((header, _)) = crate::protocol::VideoChunkHeader::decode(&packet) {
                        shards.push(crate::transport::PacerShard {
                            datagram: packet,
                            frame_id: header.frame_id,
                            chunk_idx: header.chunk_idx,
                            parity: header.is_parity(),
                            weighted_parity: header.flags & crate::protocol::video_flags::WEIGHTED_PARITY != 0,
                        });
                    }
                }
                let complete = frame_bytes <= conn.datagram_send_buffer_space().saturating_sub(16 * 1200)
                    && pacer.send_frame(
                        shards,
                        frame.captured_at + max_frame_age,
                        crate::transport::frame_budget(frame.fps) * 3 / 5,
                    );
                if complete {
                    if frame.is_keyframe {
                        last_keyframe_sent = Some(Instant::now());
                        video_recovery.accepted_keyframe();
                    }
                    last_video_frame = Some(frame.frame_id);
                    counters.video_frames_tx.fetch_add(1, Ordering::Relaxed);
                } else {
                    video_recovery.request_keyframe(&event_tx, "video datagram send failed");
                }
            }
            Some(audio) = audio_rx.recv() => {
                let bytes = audio.encode();
                if conn.send_datagram(bytes).is_ok() {
                    counters.audio_packets_tx.fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(input) = input_rx.recv() => {
                let bytes = input.encode();
                let controller = matches!(input.event, crate::protocol::InputEvent::GamepadState { .. });
                if controller || input.is_high_frequency_motion() && conn.max_datagram_size().is_some_and(|m| m >= bytes.len()) {
                    if conn.send_datagram(bytes).is_ok() {
                        counters.input_events_tx.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    let mut framed = BytesMut::with_capacity(4 + bytes.len());
                    framed.put_u32(bytes.len() as u32);
                    framed.extend_from_slice(&bytes);
                    input_stream
                        .write_all(&framed)
                        .await
                        .map_err(|e| NetworkError::Io(io::Error::other(e)))?;
                    counters.input_events_tx.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(()) = haptics_rx.changed(), if role == SessionRole::Host => {
                let update = *haptics_rx.borrow_and_update();
                let _ = conn.send_datagram(update.encode());
            }
            Some(ctrl) = control_rx.recv() => {
                let is_disc = matches!(ctrl, ControlMessage::Disconnect { .. });
                let _ = send_framed_control(&mut ctrl_stream, &ctrl).await;
                if is_disc {
                    break Ok(());
                }
            }
            Some(ctrl) = internal_ctrl_rx.recv() => {
                let _ = send_framed_control(&mut ctrl_stream, &ctrl).await;
            }
            Some(mut packet) = feedback_rx.recv() => {
                let now = Instant::now();
                if role == SessionRole::Host {
                    packet.loss_permille = packet.loss_permille.max(path_loss_permille);
                    // The pacer stamps the true departure instant.  Flush it
                    // before evaluating the samples carried in this report.
                    for sent in pacer.drain_sent() {
                        gcc.record_sent(sent.frame_id, sent.chunk_idx, sent.parity, sent.bytes, sent.sent);
                    }
                    let decision = gcc.on_feedback_at(&packet, now);
                    last_feedback = Some(now);

                    if live_fec_group != 0 {
                        adaptive_fec.observe(
                            crate::transport::FecSignal {
                                loss_permille: packet.loss_permille,
                                ack_loss_permille: gcc.ack_loss_permille(),
                                jitter_us: packet.recv_jitter_us,
                                rtt_ms: (packet.rtt_us / 1000).min(u32::from(u16::MAX)) as u16,
                            },
                            now,
                        );
                    }
                    gcc_decision = Some(decision);

                    let action = recovery.on_feedback(
                        crate::encoder_recovery::FrameFeedback {
                            highest_frame_id: packet.highest_frame_id,
                            last_intact_frame_id: packet.last_intact_frame_id,
                            nack_bitmap: packet.nack_bitmap,
                            flags: packet.flags,
                            loss_permille: packet.loss_permille,
                        },
                        now,
                    );
                    match action {
                        crate::encoder_recovery::RecoveryAction::None => {}
                        crate::encoder_recovery::RecoveryAction::IntraRefresh { frames } => {
                            let _ = event_tx.send(NetworkEvent::IntraRefreshRequested {
                                frames,
                                reason: format!(
                                    "feedback gap {} frame(s)",
                                    packet.highest_frame_id.saturating_sub(packet.last_intact_frame_id)
                                ),
                            });
                        }
                        crate::encoder_recovery::RecoveryAction::UseLongTermReference { .. } => {}
                        crate::encoder_recovery::RecoveryAction::IdrFallback => {
                            let _ = event_tx.send(NetworkEvent::KeyframeRequested {
                                last_good_frame_id: None,
                                reason: "sustained unrecoverable video loss".into(),
                            });
                        }
                    }
                }
            }
            _ = telemetry_tick.tick() => {
                let qstats = conn.stats();
                // Keep the QUIC side of the published backlog fresh even while
                // the capture loop is skipping slots and no video frame is
                // arriving to refresh it.
                overrides
                    .backlog
                    .note_quic_bytes(QUIC_DATAGRAM_BUFFER_BYTES.saturating_sub(conn.datagram_send_buffer_space()) as u64);
                let sent = qstats.path.sent_packets.saturating_sub(last_path_packets.0);
                let lost = qstats.path.lost_packets.saturating_sub(last_path_packets.1);
                if sent >= 10 {
                    path_loss_permille = (lost.saturating_mul(1000) / sent).min(1000) as u16;
                    last_path_packets = (qstats.path.sent_packets, qstats.path.lost_packets);
                } else if qstats.path.sent_packets < last_path_packets.0 {
                    last_path_packets = (0, 0);
                    path_loss_permille = 0;
                }
                let now = Instant::now();
                let (live_max, _) = overrides.bitrate_policy();
                if live_max != last_max_bitrate_bps && live_max > 0 {
                    last_max_bitrate_bps = live_max;
                    bitrate_ctrl.set_max_bitrate_bps(live_max);
                    gcc.set_max_bitrate_bps(live_max);
                }
                let feedback_fresh = last_feedback
                    .is_some_and(|stamp| now.saturating_duration_since(stamp) < Duration::from_millis(600));
                let active_bps = if feedback_fresh {
                    gcc.current_bitrate_bps()
                } else {
                    bitrate_ctrl.current_bitrate_bps()
                };
                let sample = PathTelemetrySample {
                    rtt: qstats.path.rtt,
                    min_rtt: qstats.path.min_rtt,
                    sent_packets: qstats.path.sent_packets,
                    lost_packets: qstats.path.lost_packets,
                    congestion_events: qstats.path.congestion_events,
                    peer_frames_dropped: counters.peer_frames_dropped.load(Ordering::Relaxed),
                    video_queue_delay: if keyframe_is_draining(last_keyframe_sent) {
                        Duration::ZERO
                    } else { Duration::from_secs_f64(
                        (QUIC_DATAGRAM_BUFFER_BYTES.saturating_sub(conn.datagram_send_buffer_space())
                            .saturating_add(pacer.stats().queue_bytes as usize) as f64)
                            * 8.0 / overrides.target_bitrate(active_bps).max(1) as f64,
                    ) },
                };
                let mut decision = if feedback_fresh {
                    gcc_decision.clone().unwrap_or_else(|| {
                        sync_bitrate_decision(
                            &mut bitrate_ctrl,
                            &mut last_max_bitrate_bps,
                            live_max,
                            sample,
                        )
                    })
                } else {
                    sync_bitrate_decision(&mut bitrate_ctrl, &mut last_max_bitrate_bps, live_max, sample)
                };
                if feedback_fresh {
                    decision.target_bitrate_bps = gcc.current_bitrate_bps();
                }
                overrides.apply_bitrate_policy(&mut decision);
                if role == SessionRole::Host && reported_target != decision.target_bitrate_bps {
                    reported_target = decision.target_bitrate_bps;
                    let _ = event_tx.send(NetworkEvent::BitrateUpdated(decision.clone()));
                    let _ = send_framed_control(&mut ctrl_stream, &ControlMessage::BitrateUpdate {
                        target_bitrate_bps: reported_target,
                        reason: decision.reason.clone(),
                    }).await;
                }

                let pacer_stats = pacer.stats();
                if pacer_stats.send_errors > last_pacer_errors {
                    last_pacer_errors = pacer_stats.send_errors;
                    video_recovery.request_keyframe(&event_tx, "pacer datagram send failed");
                } else if video_recovery.awaiting_keyframe {
                    video_recovery.request_keyframe(&event_tx, "waiting for a replacement keyframe");
                }

                let frames_rx = counters.video_frames_rx.load(Ordering::Relaxed);
                let frames_drop = counters.video_frames_dropped.load(Ordering::Relaxed);
                let fec_rec = counters.fec_recovered_shards.load(Ordering::Relaxed);

                let _ = send_framed_control(&mut ctrl_stream, &ControlMessage::TelemetryFeedback {
                    rtt_ms: decision.rtt_ms,
                    loss_pct: decision.loss_pct,
                    frames_received: frames_rx,
                    frames_dropped: frames_drop,
                    fec_recovered: fec_rec,
                    target_bitrate_bps: decision.target_bitrate_bps,
                }).await;

                publish_telemetry_snapshot(&decision, &counters, &telemetry, &event_tx).await;
            }
        }
    };

    dgram_task.abort();
    uni_task.abort();
    res
}

#[allow(clippy::too_many_arguments)]
async fn run_raw_udp_loop(
    role: SessionRole,
    config: NetworkConfig,
    socket: UdpSocket,
    mut video_rx: mpsc::Receiver<OutboundVideoFrame>,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
    mut input_rx: mpsc::Receiver<InputPacket>,
    mut control_rx: mpsc::Receiver<ControlMessage>,
    mut haptics_rx: watch::Receiver<crate::haptics::Rumble>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    telemetry: Arc<Mutex<NetworkTelemetry>>,
    mut shutdown_rx: watch::Receiver<bool>,
    overrides: RuntimeOverrides,
) -> Result<(), NetworkError> {
    let counters = SessionCounters::default();
    let (internal_ctrl_tx, mut internal_ctrl_rx) = mpsc::channel::<ControlMessage>(64);

    let hello = ControlMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        role: format!("{role:?}"),
        max_bitrate_bps: config.max_bitrate_bps,
        target_fps: config.target_fps,
        resume_key: None,
        two_shard_fec: false,
    };
    if let Ok(dgram) = hello.encode_datagram() {
        let _ = socket.send(&dgram).await;
    }

    let mut reassembler = VideoReassembler::new(Duration::from_millis(config.jitter_window_ms));
    let mut last_pli = Instant::now() - Duration::from_secs(1);
    let mut bitrate_ctrl =
        BitrateController::new(config.initial_bitrate_bps, config.max_bitrate_bps);
    let mut last_max_bitrate_bps = config.max_bitrate_bps;
    let mut reported_target = config.initial_bitrate_bps;
    let mut telemetry_tick = interval(Duration::from_millis(200));
    let mut sent_packets: u64 = 0;
    let mut last_rtt = Duration::from_millis(2);
    let mut min_rtt = Duration::from_millis(100);
    let mut last_peer_seen = Instant::now();
    let mut recv_buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    if let Ok(dgram) = (ControlMessage::Disconnect {
                        reason: "local shutdown".to_string(),
                    }).encode_datagram() {
                        let _ = socket.send(&dgram).await;
                    }
                    return Ok(());
                }
            }
            recv_res = socket.recv(&mut recv_buf) => {
                match recv_res {
                    Ok(len) if len > 0 => {
                        let pkt = &recv_buf[..len];
                        if pkt[0] == ChannelId::Control as u8 {
                            if let Ok(msg) = ControlMessage::decode_datagram(pkt) {
                                last_peer_seen = Instant::now();
                                match &msg {
                                    ControlMessage::Ping { client_ts_us } => {
                                        let _ = internal_ctrl_tx.try_send(ControlMessage::Pong {
                                            client_ts_us: *client_ts_us,
                                            peer_ts_us: now_unix_us(),
                                        });
                                    }
                                    ControlMessage::Pong { client_ts_us, .. } => {
                                        let now = now_unix_us();
                                        if now >= *client_ts_us {
                                            let rtt = Duration::from_micros((now - *client_ts_us).max(100));
                                            last_rtt = rtt;
                                            if rtt < min_rtt {
                                                min_rtt = rtt;
                                            }
                                        }
                                    }
                                    ControlMessage::RequestKeyframe { last_good_frame_id, reason } => {
                                        let _ = event_tx.send(NetworkEvent::KeyframeRequested {
                                            last_good_frame_id: *last_good_frame_id,
                                            reason: reason.clone(),
                                        });
                                    }
                                    ControlMessage::TelemetryFeedback { frames_dropped, .. } => {
                                        counters.peer_frames_dropped.store(*frames_dropped, Ordering::Relaxed);
                                    }
                                    ControlMessage::Disconnect { .. } => {
                                        let _ = event_tx.send(NetworkEvent::ControlReceived(msg));
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                                let _ = event_tx.send(NetworkEvent::ControlReceived(msg));
                            }
                        } else {
                            last_peer_seen = Instant::now();
                            handle_inbound_media_datagram(
                                pkt,
                                &mut reassembler,
                                &mut last_pli,
                                &counters,
                                &event_tx,
                                &internal_ctrl_tx,
                                None,
                                None,
                                role == SessionRole::Client,
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                    Err(e) => return Err(NetworkError::Io(e)),
                }
            }
            Some(frame) = video_rx.recv() => {
                let fec_group = overrides.fec_group_size.load(Ordering::Relaxed);
                let packets = match packetize_video(
                    frame.codec,
                    u64::from(frame.frame_id),
                    frame.timestamp_us,
                    frame.is_keyframe,
                    &frame.payload,
                    1200,
                    fec_group,
                    false,
                    frame.meta,
                ) {
                    Ok(packets) => packets,
                    Err(error) => {
                        tracing::warn!(%error, frame_id = frame.frame_id, "Dropping unpacketizable UDP video frame");
                        let _ = event_tx.send(NetworkEvent::KeyframeRequested {
                            last_good_frame_id: None,
                            reason: "video frame exceeded UDP transport limits".into(),
                        });
                        continue;
                    }
                };
                for pkt in packets {
                    if socket.send(&pkt).await.is_ok() {
                        sent_packets += 1;
                    }
                }
                counters.video_frames_tx.fetch_add(1, Ordering::Relaxed);
            }
            Some(audio) = audio_rx.recv() => {
                let bytes = audio.encode();
                if socket.send(&bytes).await.is_ok() {
                    sent_packets += 1;
                    counters.audio_packets_tx.fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(input) = input_rx.recv() => {
                let bytes = input.encode();
                if socket.send(&bytes).await.is_ok() {
                    sent_packets += 1;
                    counters.input_events_tx.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(()) = haptics_rx.changed(), if role == SessionRole::Host => {
                let update = *haptics_rx.borrow_and_update();
                let _ = socket.send(&update.encode()).await;
            }
            Some(ctrl) = control_rx.recv() => {
                let is_disc = matches!(ctrl, ControlMessage::Disconnect { .. });
                if let Ok(dgram) = ctrl.encode_datagram() {
                    let _ = socket.send(&dgram).await;
                }
                if is_disc {
                    return Ok(());
                }
            }
            Some(ctrl) = internal_ctrl_rx.recv() => {
                if let Ok(dgram) = ctrl.encode_datagram() {
                    let _ = socket.send(&dgram).await;
                }
            }
            _ = telemetry_tick.tick() => {
                if last_peer_seen.elapsed() > Duration::from_secs(10) {
                    return Err(NetworkError::HolePunchTimeout);
                }
                if let Ok(ping) = (ControlMessage::Ping { client_ts_us: now_unix_us() }).encode_datagram() {
                    let _ = socket.send(&ping).await;
                }

                let frames_drop = counters.video_frames_dropped.load(Ordering::Relaxed);
                let fec_rec = counters.fec_recovered_shards.load(Ordering::Relaxed);
                let frames_rx = counters.video_frames_rx.load(Ordering::Relaxed);
                let sample = PathTelemetrySample {
                    rtt: last_rtt,
                    min_rtt: min_rtt.min(last_rtt),
                    sent_packets,
                    lost_packets: frames_drop,
                    congestion_events: 0,
                    peer_frames_dropped: counters.peer_frames_dropped.load(Ordering::Relaxed),
                    video_queue_delay: Duration::ZERO,
                };
                let (live_max, _) = overrides.bitrate_policy();
                let mut decision = sync_bitrate_decision(
                    &mut bitrate_ctrl,
                    &mut last_max_bitrate_bps,
                    live_max,
                    sample,
                );
                overrides.apply_bitrate_policy(&mut decision);
                if role == SessionRole::Host && reported_target != decision.target_bitrate_bps {
                    reported_target = decision.target_bitrate_bps;
                    let _ = event_tx.send(NetworkEvent::BitrateUpdated(decision.clone()));
                }

                if let Ok(fb) = (ControlMessage::TelemetryFeedback {
                    rtt_ms: decision.rtt_ms,
                    loss_pct: decision.loss_pct,
                    frames_received: frames_rx,
                    frames_dropped: frames_drop,
                    fec_recovered: fec_rec,
                    target_bitrate_bps: decision.target_bitrate_bps,
                }).encode_datagram() {
                    let _ = socket.send(&fb).await;
                }

                publish_telemetry_snapshot(&decision, &counters, &telemetry, &event_tx).await;
            }
        }
    }
}

async fn send_framed_control(
    stream: &mut quinn::SendStream,
    msg: &ControlMessage,
) -> Result<(), NetworkError> {
    let framed = msg.encode_framed()?;
    stream
        .write_all(&framed)
        .await
        .map_err(|e| NetworkError::Io(io::Error::other(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AudioCodec, AudioPacketHeader, InputEvent, VideoCodec, audio_flags};
    use crate::signaling::LocalSignalingServer;

    #[test]
    fn capture_queue_age_tracks_frame_rate() {
        assert_eq!(max_capture_queue_age(60), Duration::from_millis(30));
        let at_120 = max_capture_queue_age(120);
        assert!(at_120 >= Duration::from_millis(16));
        assert!(at_120 <= Duration::from_millis(17));
        assert_eq!(max_capture_queue_age(144), Duration::from_millis(13));
    }

    #[tokio::test]
    async fn quic_rebind_preserves_the_connection_and_datagrams() {
        let identity = load_identity(None).unwrap();
        let server = Endpoint::server(
            build_server_config(&identity, None).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(
            build_client_config(&identity.fingerprint_hex, None).unwrap(),
        );
        let connecting = client
            .connect(server.local_addr().unwrap(), "vmax.peer")
            .unwrap();
        let (client_conn, server_conn) =
            tokio::join!(connecting, async { server.accept().await.unwrap().await });
        let (client_conn, server_conn) = (client_conn.unwrap(), server_conn.unwrap());
        let old = client.local_addr().unwrap();
        crate::nat_mobility::rebind_endpoint(&client, "127.0.0.1:0".parse().unwrap()).unwrap();
        assert_ne!(old, client.local_addr().unwrap());
        client_conn
            .send_datagram(Bytes::from_static(b"after-rebind"))
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), server_conn.read_datagram())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"after-rebind")
        );
    }

    #[tokio::test]
    async fn probing_prefers_a_fast_known_route_over_an_unmeasured_reply() {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidate = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remapped = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let expected = candidate.local_addr().unwrap();
        let tag = [19; 32];
        let responder = tokio::spawn(async move {
            let mut packet = [0; 64];
            let (_, peer) = candidate.recv_from(&mut packet).await.unwrap();
            let mut ack = PUNCH_ACK_MAGIC.to_vec();
            ack.extend_from_slice(&tag);
            remapped.send_to(&ack, peer).await.unwrap();
            tokio::time::sleep(Duration::from_millis(8)).await;
            candidate.send_to(&ack, peer).await.unwrap();
        });
        let (_keep_open, shutdown) = watch::channel(false);
        let selected = hole_punch_udp(
            &client,
            &[expected],
            Duration::from_secs(1),
            Some(tag),
            shutdown,
        )
        .await
        .unwrap();
        assert_eq!(selected, expected);
        responder.await.unwrap();
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TestRoute {
        Direct,
        DirectIpv6,
        DirectIpv4FromDualStack,
        UnreachableUdp,
        RelayUpgrade,
        RelayOnly,
    }

    async fn assert_multiplexed_loopback(protocol: TransportProtocol, route: TestRoute) {
        let mut relay = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();

        let cfg = NetworkConfig {
            signaling_url: relay.ws_url.clone(),
            enable_stun: false,
            bind_addr: if route == TestRoute::DirectIpv6 {
                "[::1]:0".parse().unwrap()
            } else {
                "127.0.0.1:0".parse().unwrap()
            },
            hole_punch_timeout_ms: if route == TestRoute::UnreachableUdp {
                250
            } else if route == TestRoute::RelayUpgrade {
                0
            } else {
                3_000
            },
            relay_only: route == TestRoute::RelayOnly,
            protocol,
            ..Default::default()
        };

        let mut host = start_host_session(cfg.clone()).await.unwrap();

        let invite_uri = loop {
            match timeout(Duration::from_secs(5), host.event_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                NetworkEvent::InviteReady { invite_uri, .. } => break invite_uri,
                _ => continue,
            }
        };

        let mut client_cfg = cfg;
        client_cfg.relay_only = false;
        if route == TestRoute::UnreachableUdp {
            client_cfg.bind_addr = "[::1]:0".parse().unwrap();
        } else if matches!(
            route,
            TestRoute::DirectIpv6 | TestRoute::DirectIpv4FromDualStack
        ) {
            client_cfg.bind_addr = NetworkConfig::default().bind_addr;
            client_cfg.advertised_signaling_url = Some("wss://relay.example.com".into());
        }
        let mut client = start_client_session(client_cfg, invite_uri).await.unwrap();

        async fn wait_connected(rx: &mut mpsc::UnboundedReceiver<NetworkEvent>, route: TestRoute) {
            loop {
                match timeout(Duration::from_secs(6), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    NetworkEvent::PhaseChanged(ConnectionPhase::Connected) => break,
                    NetworkEvent::PhaseChanged(ConnectionPhase::HolePunching)
                        if route == TestRoute::RelayOnly =>
                    {
                        panic!("relay-only session tried direct UDP")
                    }
                    NetworkEvent::PhaseChanged(ConnectionPhase::Failed) => {
                        panic!("session entered Failed phase")
                    }
                    _ => continue,
                }
            }
        }

        tokio::join!(
            wait_connected(&mut host.event_rx, route),
            wait_connected(&mut client.event_rx, route)
        );
        if route == TestRoute::RelayUpgrade {
            timeout(Duration::from_secs(4), async {
                loop {
                    if !host.telemetry.lock().await.relayed
                        && !client.telemetry.lock().await.relayed
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("peers did not upgrade the live connection");
        }
        let relayed = matches!(route, TestRoute::UnreachableUdp | TestRoute::RelayOnly);
        assert_eq!(host.telemetry.lock().await.relayed, relayed);
        let client_snapshot = client.telemetry.lock().await.clone();
        assert_eq!(client_snapshot.relayed, relayed);
        if matches!(
            route,
            TestRoute::DirectIpv6 | TestRoute::DirectIpv4FromDualStack
        ) {
            assert!(client_snapshot.local_udp_addr.unwrap().is_ipv6());
            assert_eq!(
                client_snapshot.active_peer_addr.unwrap().is_ipv6(),
                route == TestRoute::DirectIpv6
            );
        }

        let synthetic_hevc = Bytes::from(vec![0xAA; 24_000]);
        if protocol == TransportProtocol::Quic {
            host.tx
                .video_tx
                .send(OutboundVideoFrame {
                    meta: crate::telemetry::FrameTelemetry::full_range_hardware(1, 1920, 1080),
                    codec: VideoCodec::Hevc,
                    frame_id: 0,
                    timestamp_us: 0,
                    captured_at: Instant::now() - Duration::from_millis(100),
                    fps: 60,
                    is_keyframe: true,
                    payload: Bytes::from_static(b"obsolete frame"),
                })
                .await
                .unwrap();
        }
        host.tx
            .video_tx
            .send(OutboundVideoFrame {
                meta: crate::telemetry::FrameTelemetry::full_range_hardware(1, 1920, 1080),
                codec: VideoCodec::Hevc,
                frame_id: 1,
                timestamp_us: 1000,
                captured_at: Instant::now(),
                fps: 60,
                is_keyframe: true,
                payload: synthetic_hevc.clone(),
            })
            .await
            .unwrap();

        host.tx
            .audio_tx
            .send(AudioPacket {
                header: AudioPacketHeader {
                    codec: AudioCodec::Opus as u8,
                    flags: audio_flags::DENOISED | audio_flags::VOICE_TRACK,
                    channels: 2,
                    seq: 7,
                    timestamp_us: 2000,
                },
                payload: Bytes::from_static(b"denoised-audio"),
            })
            .await
            .unwrap();

        client
            .tx
            .input_tx
            .send(InputPacket {
                seq: 1,
                client_ts_us: 3000,
                event: InputEvent::Key {
                    scan_code: 0x11,
                    pressed: true,
                    modifiers: 0,
                },
            })
            .await
            .unwrap();
        client
            .tx
            .input_tx
            .send(InputPacket {
                seq: 2,
                client_ts_us: 3100,
                event: InputEvent::MouseMoveRel { dx: 12, dy: -4 },
            })
            .await
            .unwrap();

        client
            .tx
            .input_tx
            .send(InputPacket {
                seq: 3,
                client_ts_us: 3200,
                event: InputEvent::GamepadState {
                    index: 0,
                    buttons: 0x1008,
                    left_trigger: 127,
                    right_trigger: 255,
                    left_x: -32768,
                    left_y: 32767,
                    right_x: 0,
                    right_y: -4096,
                },
            })
            .await
            .unwrap();

        let rumble = crate::haptics::Rumble {
            large: 192,
            small: 54,
            pad: 0,
        };
        host.tx.haptics_tx.send_replace(rumble);
        let mut got_video = false;
        let mut got_audio = false;
        let mut got_rumble = false;
        let deadline = Instant::now() + Duration::from_secs(4);
        while !(got_video && got_audio && got_rumble) && Instant::now() < deadline {
            if let Ok(Some(ev)) = timeout(Duration::from_millis(250), client.event_rx.recv()).await
            {
                match ev {
                    NetworkEvent::VideoFrameReceived(frame) => {
                        assert_eq!(frame.frame_id, 1);
                        assert_eq!(frame.codec, VideoCodec::Hevc as u8);
                        assert_eq!(frame.payload, synthetic_hevc);
                        got_video = true;
                    }
                    NetworkEvent::AudioPacketReceived(pkt) => {
                        assert_eq!(pkt.header.seq, 7);
                        assert_eq!(pkt.payload.as_ref(), b"denoised-audio");
                        got_audio = true;
                    }
                    NetworkEvent::RumbleReceived(packet) => {
                        assert_eq!(packet, rumble);
                        got_rumble = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(got_video, "client did not receive video frame");
        assert!(got_audio, "client did not receive audio packet");
        assert!(got_rumble, "client did not receive haptic feedback");
        host.tx
            .haptics_tx
            .send_replace(crate::haptics::Rumble::default());
        timeout(Duration::from_secs(2), async {
            while let Some(event) = client.event_rx.recv().await {
                if let NetworkEvent::RumbleReceived(packet) = event {
                    assert_eq!(packet, crate::haptics::Rumble::default());
                    return;
                }
            }
            panic!("session ended before stop rumble arrived");
        })
        .await
        .expect("client did not receive stop rumble");

        let mut got_key = false;
        let mut got_mouse = false;
        let mut got_gamepad = false;
        let deadline = Instant::now() + Duration::from_secs(4);
        while !(got_key && got_mouse && got_gamepad) && Instant::now() < deadline {
            if let Ok(Some(NetworkEvent::InputEventReceived(pkt))) =
                timeout(Duration::from_millis(250), host.event_rx.recv()).await
            {
                match pkt.event {
                    InputEvent::Key {
                        scan_code: 0x11, ..
                    } => got_key = true,
                    InputEvent::MouseMoveRel { dx: 12, dy: -4 } => got_mouse = true,
                    InputEvent::GamepadState {
                        index: 0,
                        buttons: 0x1008,
                        left_trigger: 127,
                        right_trigger: 255,
                        left_x: -32768,
                        left_y: 32767,
                        right_x: 0,
                        right_y: -4096,
                    } => {
                        assert_eq!(pkt.seq, 3);
                        got_gamepad = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(got_key, "host did not receive reliable key input");
        assert!(got_mouse, "host did not receive fast mouse delta");
        assert!(got_gamepad, "host did not receive the controller report");

        if protocol == TransportProtocol::Quic && route == TestRoute::Direct {
            while host.event_rx.try_recv().is_ok() {}
            let frame = |frame_id, is_keyframe| OutboundVideoFrame {
                meta: crate::telemetry::FrameTelemetry::full_range_hardware(1, 1920, 1080),
                codec: VideoCodec::Hevc,
                frame_id,
                timestamp_us: u64::from(frame_id) * 16_667,
                captured_at: Instant::now(),
                fps: 60,
                is_keyframe,
                payload: synthetic_hevc.clone(),
            };
            // Skip frame 2, simulating a capture-to-network queue drop.
            host.tx.video_tx.send(frame(3, false)).await.unwrap();
            timeout(Duration::from_secs(2), async {
                while let Some(event) = host.event_rx.recv().await {
                    if let NetworkEvent::KeyframeRequested {
                        last_good_frame_id,
                        reason,
                    } = event
                        && reason == "video queue lost a reference frame"
                    {
                        assert_eq!(last_good_frame_id, None, "recovery must request an IDR");
                        return;
                    }
                }
                panic!("host event channel closed");
            })
            .await
            .expect("queue drop should request an IDR");

            host.tx.video_tx.send(frame(4, true)).await.unwrap();
            timeout(Duration::from_secs(2), async {
                while let Some(event) = client.event_rx.recv().await {
                    if let NetworkEvent::VideoFrameReceived(frame) = event {
                        assert_eq!(frame.frame_id, 4);
                        assert!(frame.is_keyframe);
                        return;
                    }
                }
                panic!("client event channel closed");
            })
            .await
            .expect("client should receive the replacement keyframe");
        }

        client.tx.close();
        timeout(Duration::from_secs(2), client.join_handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if protocol == TransportProtocol::Quic {
            timeout(Duration::from_secs(2), host.join_handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            host.tx.close();
            let _ = host.join_handle.await;
        }
        relay.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_direct_and_relay_streams_survive_repeated_frame_gaps() {
        let mut relay = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        let test_session = |relay_only| {
            let endpoint = relay.ws_url.clone();
            async move {
                let config = NetworkConfig {
                    signaling_url: endpoint,
                    bind_addr: "127.0.0.1:0".parse().unwrap(),
                    enable_stun: false,
                    relay_only,
                    fixed_bitrate: true,
                    initial_bitrate_bps: 25_000_000,
                    max_bitrate_bps: 25_000_000,
                    ..Default::default()
                };
                let mut host = start_host_session(config.clone()).await.unwrap();
                let invite = timeout(Duration::from_secs(5), async {
                    while let Some(event) = host.event_rx.recv().await {
                        if let NetworkEvent::InviteReady { invite_uri, .. } = event {
                            return invite_uri;
                        }
                    }
                    panic!("host closed before publishing its invite");
                })
                .await
                .unwrap();
                let mut client = start_client_session(config, invite).await.unwrap();
                for session in [&mut host, &mut client] {
                    timeout(Duration::from_secs(8), async {
                        while let Some(event) = session.event_rx.recv().await {
                            match event {
                                NetworkEvent::PhaseChanged(ConnectionPhase::Connected) => return,
                                NetworkEvent::PhaseChanged(ConnectionPhase::Failed) => {
                                    panic!("session failed during setup")
                                }
                                _ => {}
                            }
                        }
                        panic!("session ended during setup");
                    })
                    .await
                    .unwrap();
                }

                let mut cadence = interval(Duration::from_nanos(16_666_667));
                cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let payload = Bytes::from(vec![0x35; 24_000]);
                let video_tx = host.tx.video_tx.clone();
                let audio_tx = host.tx.audio_tx.clone();
                let produce = async move {
                    for id in 0..480u32 {
                        cadence.tick().await;
                        // Skip one reference frame each second. A new keyframe
                        // after the gap models the host's recovery response.
                        if id % 60 != 48 {
                            video_tx
                                .send(OutboundVideoFrame {
                                    frame_id: id,
                                    codec: VideoCodec::H264,
                                    meta: crate::telemetry::FrameTelemetry::full_range_hardware(
                                        0, 1920, 1080,
                                    ),
                                    timestamp_us: u64::from(id) * 16_667,
                                    captured_at: Instant::now(),
                                    fps: 60,
                                    is_keyframe: matches!(id % 60, 0 | 50),
                                    payload: payload.clone(),
                                })
                                .await
                                .unwrap();
                        }
                        audio_tx
                            .send(AudioPacket {
                                header: AudioPacketHeader {
                                    codec: AudioCodec::Opus as u8,
                                    flags: 0,
                                    channels: 2,
                                    seq: id,
                                    timestamp_us: u64::from(id) * 16_667,
                                },
                                payload: Bytes::from_static(b"audio"),
                            })
                            .await
                            .unwrap();
                    }
                };

                let consume = async {
                    let mut video = 0;
                    let mut audio = 0;
                    let mut replacements = 0;
                    let mut last_frame = None;
                    while let Some(event) = client.event_rx.recv().await {
                        match event {
                            NetworkEvent::VideoFrameReceived(frame) => {
                                assert!(last_frame.is_none_or(|id| id < frame.frame_id));
                                last_frame = Some(frame.frame_id);
                                video += 1;
                                if frame.frame_id % 60 == 50 {
                                    assert!(frame.is_keyframe);
                                    replacements += 1;
                                }
                            }
                            NetworkEvent::AudioPacketReceived(_) => audio += 1,
                            NetworkEvent::PhaseChanged(
                                ConnectionPhase::Failed | ConnectionPhase::Disconnected,
                            ) => panic!("transport closed during load"),
                            _ => {}
                        }
                        if matches!(last_frame, Some(479)) && audio >= 460 {
                            break;
                        }
                    }
                    eprintln!(
                        "relayed={relay_only}: {video} video frames, {audio} audio packets, {replacements} replacement keyframes"
                    );
                    assert!(
                        video >= 420,
                        "too few video frames: {video}, relayed={relay_only}"
                    );
                    assert!(
                        audio >= 460,
                        "too few audio packets: {audio}, relayed={relay_only}"
                    );
                    assert_eq!(
                        replacements, 8,
                        "a replacement keyframe was lost, relayed={relay_only}"
                    );
                };
                timeout(Duration::from_secs(15), async {
                    tokio::join!(produce, consume);
                })
                .await
                .expect("stream stalled under load");
                client.tx.close();
                timeout(Duration::from_secs(2), async {
                    client.join_handle.await.unwrap().unwrap();
                    host.join_handle.await.unwrap().unwrap();
                })
                .await
                .unwrap();
            }
        };
        tokio::join!(test_session(false), test_session(true), test_session(true));
        relay.stop();
    }

    #[tokio::test]
    async fn local_code_without_discovery_never_dials_loopback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = NetworkConfig {
            signaling_url: format!("ws://{}", listener.local_addr().unwrap()),
            ..Default::default()
        };
        let client = start_client_session(config, generate_room_code())
            .await
            .unwrap();
        let error = tokio::select! {
            _ = listener.accept() => panic!("The client attempted to use its own loopback endpoint"),
            result = timeout(Duration::from_secs(4), client.join_handle) => result.unwrap().unwrap().unwrap_err(),
        };
        assert!(matches!(error, NetworkError::LocalRoomNotFound));
    }

    #[tokio::test]
    async fn host_and_client_connect_via_signaling_and_exchange_multiplexed_packets() {
        assert_multiplexed_loopback(TransportProtocol::Quic, TestRoute::Direct).await;
    }

    #[tokio::test]
    async fn dual_stack_client_connects_to_ipv6_host() {
        assert_multiplexed_loopback(TransportProtocol::Quic, TestRoute::DirectIpv6).await;
    }

    #[tokio::test]
    async fn dual_stack_client_connects_to_ipv4_host() {
        assert_multiplexed_loopback(TransportProtocol::Quic, TestRoute::DirectIpv4FromDualStack)
            .await;
    }

    #[tokio::test]
    async fn host_and_client_raw_udp_loopback_exchanges_multiplexed_packets() {
        assert_multiplexed_loopback(TransportProtocol::RawUdp, TestRoute::Direct).await;
    }

    #[tokio::test]
    async fn quic_falls_back_to_websocket_when_peer_udp_is_unreachable() {
        assert_multiplexed_loopback(TransportProtocol::Quic, TestRoute::UnreachableUdp).await;
    }

    #[tokio::test]
    async fn relayed_session_upgrades_to_direct_udp_and_exchanges_media() {
        assert_multiplexed_loopback(TransportProtocol::Quic, TestRoute::RelayUpgrade).await;
    }

    #[tokio::test]
    async fn relay_only_host_uses_authenticated_quic_even_when_direct_udp_is_available() {
        assert_multiplexed_loopback(TransportProtocol::RawUdp, TestRoute::RelayOnly).await;
    }

    #[tokio::test]
    async fn saved_session_requires_approval_and_reuses_verified_identities() {
        async fn wait_for(handle: &SessionHandle, ready: impl Fn(&NetworkTelemetry) -> bool) {
            timeout(Duration::from_secs(8), async {
                loop {
                    {
                        let snapshot = handle.telemetry.lock().await;
                        assert_ne!(
                            snapshot.phase,
                            ConnectionPhase::Failed,
                            "{:?}",
                            snapshot.last_error
                        );
                        if ready(&snapshot) {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }

        let server = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        let nonce = rand::random::<u64>();
        let host_file = std::env::temp_dir().join(format!("vmax-test-{nonce}-host.json"));
        let client_file = std::env::temp_dir().join(format!("vmax-test-{nonce}-client.json"));
        let host_cfg = NetworkConfig {
            signaling_url: server.ws_url.clone(),
            enable_stun: false,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            identity_file: Some(host_file.clone()),
            host_room_code: Some(generate_room_code()),
            ..NetworkConfig::default()
        };
        let client_cfg = NetworkConfig {
            identity_file: Some(client_file.clone()),
            ..host_cfg.clone()
        };
        let host = start_host_session(host_cfg.clone()).await.unwrap();
        wait_for(&host, |snapshot| snapshot.invite_uri.is_some()).await;
        let uri = host.telemetry.lock().await.invite_uri.clone().unwrap();
        let client = start_client_session(client_cfg.clone(), uri.clone())
            .await
            .unwrap();
        wait_for(&client, |snapshot| snapshot.peer_savable).await;
        wait_for(&host, |snapshot| snapshot.peer_savable).await;
        let saved_host = client.telemetry.lock().await.trusted_peer.clone().unwrap();
        let saved_client = host.telemetry.lock().await.trusted_peer.clone().unwrap();
        let resume_key = saved_host.resume_key.clone().unwrap();
        assert_eq!(
            saved_host.fingerprint,
            load_identity(Some(host_file.clone()))
                .unwrap()
                .fingerprint_hex
        );
        assert_eq!(
            saved_client.fingerprint,
            load_identity(Some(client_file.clone()))
                .unwrap()
                .fingerprint_hex
        );
        assert!(
            !serde_json::to_string(&*client.telemetry.lock().await)
                .unwrap()
                .contains(&resume_key)
        );
        client.tx.close();
        host.tx.close();
        let _ = client.join_handle.await;
        let _ = host.join_handle.await;

        let rejoined = start_client_session(client_cfg.clone(), uri.clone())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let rehosted = start_host_session(host_cfg.clone()).await.unwrap();
        wait_for(&rejoined, |snapshot| {
            snapshot.phase == ConnectionPhase::Connected
        })
        .await;
        assert_eq!(
            rehosted.telemetry.lock().await.invite_uri.as_deref(),
            Some(uri.as_str())
        );
        rejoined.tx.close();
        rehosted.tx.close();
        let _ = rejoined.join_handle.await;
        let _ = rehosted.join_handle.await;

        use crate::signaling::FriendCommand;
        let endpoint = &server.ws_url;
        let request = FriendCommand::Request {
            host: saved_host.fingerprint.clone(),
        };
        assert!(
            !friend_relay_command(client_file.clone(), endpoint, request.clone())
                .await
                .unwrap()
                .1
        );
        friend_relay_command(client_file.clone(), endpoint, request)
            .await
            .unwrap();
        let inbox = FriendCommand::Inbox { clients: vec![] };
        assert!(
            friend_relay_command(client_file.clone(), endpoint, inbox.clone())
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(
            friend_relay_command(host_file.clone(), endpoint, inbox.clone())
                .await
                .unwrap()
                .0,
            vec![saved_client.fingerprint.clone()]
        );
        let status = FriendCommand::Status {
            host: saved_host.fingerprint.clone(),
        };
        assert!(
            !friend_relay_command(client_file.clone(), endpoint, status.clone())
                .await
                .unwrap()
                .1
        );
        let approved = FriendCommand::Inbox {
            clients: vec![saved_client.fingerprint.clone()],
        };
        assert!(
            friend_relay_command(host_file.clone(), endpoint, approved)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(
            friend_relay_command(client_file.clone(), endpoint, status.clone())
                .await
                .unwrap()
                .1
        );
        friend_relay_command(host_file.clone(), endpoint, inbox)
            .await
            .unwrap();
        assert!(
            !friend_relay_command(client_file.clone(), endpoint, status)
                .await
                .unwrap()
                .1
        );

        let host = start_host_session(NetworkConfig {
            saved_clients: Some(vec![saved_client.fingerprint]),
            ..host_cfg
        })
        .await
        .unwrap();
        let rejected = timeout(Duration::from_secs(4), async {
            loop {
                let result = signal_as_client(
                    &server.ws_url,
                    format!("saved-{resume_key}"),
                    "f".repeat(64),
                    Vec::new(),
                )
                .await;
                match result {
                    Err(SignalingError::RemoteError(message)) if message.contains("offline") => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(SignalingError::RemoteError(message)) => break message,
                    _ => panic!("unapproved client was not rejected"),
                }
            }
        })
        .await
        .unwrap();
        assert!(rejected.contains("no permission"));
        let client = start_saved_client_session(client_cfg, saved_host.fingerprint, resume_key)
            .await
            .unwrap();
        wait_for(&client, |snapshot| {
            snapshot.saved_session && snapshot.peer_savable
        })
        .await;
        wait_for(&host, |snapshot| {
            snapshot.saved_session && snapshot.peer_savable
        })
        .await;
        assert!(host.telemetry.lock().await.invite_uri.is_none());
        client.tx.close();
        host.tx.close();
        let _ = client.join_handle.await;
        let _ = host.join_handle.await;
        std::fs::remove_file(host_file).unwrap();
        std::fs::remove_file(client_file).unwrap();
    }
}
