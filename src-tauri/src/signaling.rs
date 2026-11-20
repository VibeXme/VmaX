use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, interval, timeout, timeout_at};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message, protocol::WebSocketConfig};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async_with_config, client_async_tls_with_config,
};
use tracing::{debug, info, warn};
use url::Url;

#[derive(Debug, Error)]
pub enum SignalingError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("invalid signaling URL '{0}': {1}")]
    InvalidUrl(String, String),
    #[error("invalid invite code or link: {0}")]
    InvalidInvite(String),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("signaling server error: {0}")]
    RemoteError(String),
    #[error("signaling channel closed before handshake completed")]
    Closed,
    #[error("signaling server did not respond in time")]
    TimedOut,
    #[error("{stage} failed for {endpoint}: {source}")]
    EndpointIo {
        endpoint: String,
        stage: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(
        "TCP connection refused by {endpoint}. Check that the host or public VmaX relay is running on this port, then use a fresh invite. ({source})"
    )]
    EndpointRefused {
        endpoint: String,
        #[source]
        source: io::Error,
    },
    #[error("{stage} timed out for {endpoint}")]
    EndpointTimeout {
        endpoint: String,
        stage: &'static str,
    },
    #[error("{stage} failed for {endpoint}: {detail}")]
    EndpointHandshake {
        endpoint: String,
        stage: &'static str,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalingMessage {
    CreateRoom {
        room_code: String,
        cert_fingerprint: String,
        candidates: Vec<SocketAddr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_candidate: Option<SocketAddr>,
        #[serde(default)]
        relay: bool,
        #[serde(default)]
        raw_udp: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_clients: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_name: Option<String>,
    },
    RoomCreated {
        room_code: String,
    },
    JoinRoom {
        room_code: String,
        cert_fingerprint: String,
        candidates: Vec<SocketAddr>,
        #[serde(default)]
        relay: bool,
    },
    /// Asks the relay for the list of currently joinable rooms. Sent as the
    /// first message on a fresh connection; the server answers and closes.
    ListRooms,
    RoomListing {
        rooms: Vec<RoomSummary>,
    },
    PeerMatched {
        role: String,
        peer_fingerprint: String,
        peer_candidates: Vec<SocketAddr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_candidate: Option<SocketAddr>,
        #[serde(default)]
        relay: bool,
        #[serde(default)]
        raw_udp: Option<bool>,
    },
    DirectPath {
        ready: bool,
    },
    FriendCommand {
        certificate: Vec<u8>,
        command: FriendCommand,
    },
    FriendChallenge {
        nonce: [u8; 32],
    },
    FriendProof {
        signature: Vec<u8>,
    },
    FriendReply {
        requests: Vec<String>,
        approved: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FriendCommand {
    Request { host: String },
    Inbox { clients: Vec<String> },
    Resolve { client: String },
    Status { host: String },
}

pub(crate) fn friend_proof_payload(
    nonce: &[u8; 32],
    command: &FriendCommand,
) -> Result<Vec<u8>, SignalingError> {
    let mut payload = b"vmax-friend-command-v1\0".to_vec();
    payload.extend_from_slice(nonce);
    payload.extend(serde_json::to_vec(command)?);
    Ok(payload)
}

#[derive(Default)]
struct FriendMailbox {
    // Entries are indexed by destination host and authenticated requester.
    entries: HashMap<(String, String), Instant>,
    access: HashMap<String, (Instant, Vec<String>)>,
}

impl FriendMailbox {
    fn apply(&mut self, identity: String, command: FriendCommand) -> SignalingMessage {
        let now = Instant::now();
        self.entries.retain(|_, expires| *expires > now);
        self.access
            .retain(|_, (expires, _)| *expires > Instant::now());
        let expires = Instant::now() + Duration::from_secs(24 * 60 * 60);
        let mut requests = Vec::new();
        let mut approved = false;
        match command {
            FriendCommand::Request { host } if is_fingerprint(&host) && host != identity => {
                let key = (host.to_ascii_lowercase(), identity);
                approved = self
                    .access
                    .get(&key.0)
                    .is_some_and(|(_, clients)| clients.contains(&key.1));
                if !approved && !self.entries.contains_key(&key) {
                    if self.entries.len() >= 4096
                        || self
                            .entries
                            .keys()
                            .filter(|(host, _)| host == &key.0)
                            .count()
                            >= 128
                        || self
                            .entries
                            .keys()
                            .filter(|(_, client)| client == &key.1)
                            .count()
                            >= 64
                    {
                        return SignalingMessage::Error {
                            message: "Friend request inbox is full. Try again later.".into(),
                        };
                    }
                    self.entries.insert(key, expires);
                }
            }
            FriendCommand::Inbox { clients }
                if clients.len() <= 64 && clients.iter().all(|client| is_fingerprint(client)) =>
            {
                self.entries
                    .retain(|(host, client), _| host != &identity || !clients.contains(client));
                requests = self
                    .entries
                    .keys()
                    .filter(|(host, _)| host == &identity)
                    .map(|(_, client)| client.clone())
                    .collect();
                requests.sort_unstable();
                if self.access.len() < 4096 || self.access.contains_key(&identity) {
                    self.access
                        .insert(identity, (now + Duration::from_secs(45), clients));
                }
            }
            FriendCommand::Resolve { client } if is_fingerprint(&client) => {
                self.entries
                    .remove(&(identity, client.to_ascii_lowercase()));
            }
            FriendCommand::Status { host } if is_fingerprint(&host) => {
                approved = self
                    .access
                    .get(&host.to_ascii_lowercase())
                    .is_some_and(|(_, clients)| clients.contains(&identity));
            }
            _ => {
                return SignalingMessage::Error {
                    message: "Invalid friend request.".into(),
                };
            }
        }
        SignalingMessage::FriendReply { requests, approved }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSummary {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedInvite {
    pub room_code: String,
    pub signaling_url: String,
    pub expected_host_fingerprint: Option<String>,
    pub raw_udp: Option<bool>,
    pub discover_lan: bool,
    pub endpoint_from_link: bool,
}

const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

pub fn generate_room_code() -> String {
    let mut rng = rand::rng();
    let mut chars = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            chars.push('-');
        }
        let idx = rng.random_range(0..CODE_ALPHABET.len());
        chars.push(CODE_ALPHABET[idx] as char);
    }
    chars
}

/// Builds a `vmax://join/<room_code>?sig=...&fp=...` invite URI suitable for one-click links & QR codes.
pub fn build_invite_uri(
    room_code: &str,
    signaling_url: &str,
    host_fingerprint: &str,
    raw_udp: bool,
) -> Result<String, SignalingError> {
    let mut url = Url::parse(&format!("vmax://join/{room_code}"))
        .map_err(|e| SignalingError::InvalidInvite(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("sig", signaling_url)
        .append_pair("fp", host_fingerprint)
        .append_pair("transport", if raw_udp { "raw_udp" } else { "quic" });
    Ok(url.to_string())
}

/// Parses either a `vmax://join/...` URI or a bare room code (`ABCD-EFGH`).
pub fn parse_invite_input(
    raw: &str,
    fallback_signaling_url: &str,
) -> Result<ParsedInvite, SignalingError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SignalingError::InvalidInvite(
            "invite input is empty".to_string(),
        ));
    }

    if trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("vmax://"))
    {
        let url = Url::parse(trimmed).map_err(|e| SignalingError::InvalidInvite(e.to_string()))?;
        if !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("join"))
        {
            return Err(SignalingError::InvalidInvite(
                "expected a vmax://join link".to_string(),
            ));
        }
        let room_code = url
            .path_segments()
            .and_then(|mut segs| segs.next().filter(|_| segs.next().is_none()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                SignalingError::InvalidInvite("missing room code in vmax:// URI".to_string())
            })?;
        let room_code = normalize_room_code(room_code)?;

        let mut link_endpoint = None;
        let mut expected_host_fingerprint = None;
        let mut raw_udp = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "sig" if !v.is_empty() => link_endpoint = Some(v.into_owned()),
                "fp" if !v.is_empty() => expected_host_fingerprint = Some(v.into_owned()),
                "transport" if v == "quic" => raw_udp = Some(false),
                "transport" if v == "raw_udp" => raw_udp = Some(true),
                _ => {}
            }
        }

        let endpoint_from_link = link_endpoint.is_some();
        let signaling_url = link_endpoint.unwrap_or_else(|| fallback_signaling_url.to_string());
        validate_signaling_url(&signaling_url)?;
        Ok(ParsedInvite {
            room_code,
            signaling_url,
            expected_host_fingerprint,
            raw_udp,
            discover_lan: false,
            endpoint_from_link,
        })
    } else {
        let endpoint = validate_signaling_url(fallback_signaling_url)?;
        Ok(ParsedInvite {
            room_code: normalize_room_code(trimmed)?,
            signaling_url: fallback_signaling_url.to_string(),
            expected_host_fingerprint: None,
            raw_udp: None,
            discover_lan: endpoint.scheme() == "ws"
                && match endpoint.host() {
                    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                    Some(url::Host::Domain(host)) => host == "localhost",
                    None => false,
                },
            endpoint_from_link: false,
        })
    }
}

pub struct PeerMatch {
    pub room_code: String,
    pub peer_fingerprint: String,
    pub peer_candidates: Vec<SocketAddr>,
    pub turn_candidate: Option<SocketAddr>,
    pub relay: Option<RelayConnection>,
    pub raw_udp: Option<bool>,
}

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_RELAY_PACKET: usize = 4096;
const LAN_DISCOVERY_PORT: u16 = 8788;
const DISCOVERY_QUERY: &[u8; 8] = b"VMAXLAN1";
const DISCOVERY_REPLY: &[u8; 8] = b"VMAXLANS";
const PRESENCE_QUERY: &[u8; 8] = b"VMAXWHO1";
const PRESENCE_REPLY: &[u8; 8] = b"VMAXWHR1";
type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ServerSocket = WebSocketStream<TcpStream>;

pub struct RelayConnection(ClientSocket);

pub fn validate_signaling_url(raw: &str) -> Result<Url, SignalingError> {
    let url = Url::parse(raw)
        .map_err(|error| SignalingError::InvalidUrl(raw.to_string(), error.to_string()))?;
    if matches!(url.scheme(), "ws" | "wss")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
    {
        Ok(url)
    } else {
        Err(SignalingError::InvalidUrl(
            raw.to_string(),
            "expected a ws:// or wss:// endpoint".to_string(),
        ))
    }
}

pub fn is_lan_signaling_url(raw: &str) -> bool {
    validate_signaling_url(raw)
        .ok()
        .is_some_and(|url| match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }
            Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".local"),
            None => false,
        })
}

/// Installs the rustls `ring` crypto provider exactly once per process.
///
/// Both the `ring` and `aws-lc-rs` rustls features are enabled through
/// transitive dependencies, so rustls cannot infer a default provider from its
/// crate features and panics ("Could not automatically determine the
/// process-level CryptoProvider") the first time a TLS config is built. Every
/// rustls entry point calls this first; repeated calls are a cheap no-op.
pub(crate) fn ensure_rustls_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub(crate) async fn connect_signaling(url: &str) -> Result<ClientSocket, SignalingError> {
    ensure_rustls_provider();
    let parsed = validate_signaling_url(url)?;
    let host = parsed.host_str().unwrap_or_default();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "wss" { 443 } else { 80 });
    // Omit paths and query strings from diagnostics, since they may carry credentials.
    let endpoint = format!("{}://{host}:{port}", parsed.scheme());
    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    let stage_timeout = |stage| SignalingError::EndpointTimeout {
        endpoint: endpoint.clone(),
        stage,
    };
    let io_failure = |stage, source| SignalingError::EndpointIo {
        endpoint: endpoint.clone(),
        stage,
        source,
    };
    let addresses: Vec<_> = timeout_at(
        deadline,
        tokio::net::lookup_host((host.trim_matches(['[', ']']), port)),
    )
    .await
    .map_err(|_| stage_timeout("DNS lookup"))?
    .map_err(|error| io_failure("DNS lookup", error))?
    .collect();
    if addresses.is_empty() {
        return Err(io_failure(
            "DNS lookup",
            io::Error::new(io::ErrorKind::NotFound, "no addresses returned"),
        ));
    }
    let tcp = timeout_at(deadline, connect_candidates(&addresses))
        .await
        .map_err(|_| stage_timeout("TCP connection"))?
        .map_err(|source| {
            if source.kind() == io::ErrorKind::ConnectionRefused {
                SignalingError::EndpointRefused {
                    endpoint: endpoint.clone(),
                    source,
                }
            } else {
                io_failure("TCP connection", source)
            }
        })?;
    tcp.set_nodelay(true)
        .map_err(|error| io_failure("TCP socket setup", error))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(16 * 1024))
        .max_frame_size(Some(16 * 1024));
    let handshake_stage = if parsed.scheme() == "wss" {
        "TLS / WebSocket handshake"
    } else {
        "WebSocket handshake"
    };
    let (socket, _) = timeout_at(
        deadline,
        client_async_tls_with_config(url, tcp, Some(config), None),
    )
    .await
    .map_err(|_| stage_timeout(handshake_stage))?
    .map_err(|error| {
        let (stage, detail) = match error {
            WebSocketError::Tls(error) => ("TLS handshake", error.to_string()),
            WebSocketError::Http(response) => {
                ("WebSocket upgrade", format!("HTTP {}", response.status()))
            }
            other => (handshake_stage, other.to_string()),
        };
        SignalingError::EndpointHandshake {
            endpoint,
            stage,
            detail,
        }
    })?;
    Ok(socket)
}

async fn connect_candidates(addresses: &[SocketAddr]) -> io::Result<TcpStream> {
    // Do not let a blackholed IPv6 route consume the entire deadline before
    // trying IPv4. Stagger a bounded set of connections and keep the first win.
    let mut attempts = JoinSet::new();
    for (index, address) in addresses.iter().copied().take(16).enumerate() {
        attempts.spawn(async move {
            if index != 0 {
                tokio::time::sleep(Duration::from_millis(250 * index as u64)).await;
            }
            #[cfg(target_os = "android")]
            if let SocketAddr::V4(peer) = address
                && let Some(source) = android_lan_source(*peer.ip())
                && let Ok(socket) = tokio::net::TcpSocket::new_v4()
                && socket.bind(SocketAddr::new(source.into(), 0)).is_ok()
                && let Ok(Ok(stream)) =
                    timeout(Duration::from_millis(500), socket.connect(address)).await
            {
                return Ok(stream);
            }
            TcpStream::connect(address).await
        });
    }
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no signaling addresses");
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(socket)) => return Ok(socket),
            Ok(Err(error)) => last = error,
            Err(error) => last = io::Error::other(error),
        }
    }
    Err(last)
}

impl RelayConnection {
    pub async fn agree_direct(&mut self, ready: bool) -> Result<bool, SignalingError> {
        timeout(SIGNAL_TIMEOUT, async {
            let status = SignalingMessage::DirectPath { ready };
            self.0
                .send(Message::Text(serde_json::to_string(&status)?.into()))
                .await?;
            while let Some(frame) = self.0.next().await {
                if let Message::Text(text) = frame?
                    && let SignalingMessage::DirectPath { ready: peer_ready } =
                        serde_json::from_str(&text)?
                {
                    return Ok(ready && peer_ready);
                }
            }
            Err(SignalingError::Closed)
        })
        .await
        .map_err(|_| SignalingError::TimedOut)?
    }

    /// Relays opaque QUIC packets between a connected loopback socket and the paired WebSocket.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn forward(self, udp: tokio::net::UdpSocket) -> Result<(), SignalingError> {
        self.forward_routes(&udp, async |_| false).await
    }

    pub(crate) async fn forward_routes(
        self,
        udp: &UdpSocket,
        try_udp: impl AsyncFn(&[u8]) -> bool,
    ) -> Result<(), SignalingError> {
        let (mut send, mut recv) = self.0.split();
        let mut buf = [0; MAX_RELAY_PACKET];
        let to_relay = async {
            loop {
                let len = udp
                    .recv(&mut buf)
                    .await
                    .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
                if try_udp(&buf[..len]).await {
                    continue;
                }
                send.send(Message::Binary(bytes::Bytes::copy_from_slice(&buf[..len])))
                    .await?;
            }
        };
        let from_relay = async {
            while let Some(frame) = recv.next().await {
                match frame? {
                    Message::Binary(packet) if packet.len() <= MAX_RELAY_PACKET => {
                        udp.send(&packet)
                            .await
                            .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Ok(())
        };
        tokio::select! {
            result = to_relay => result,
            result = from_relay => result,
        }
    }
}

/// Performs Host-side WebSocket signaling: registers room, notifies `room_ready_tx`, then waits for a peer.
pub async fn signal_as_host(
    signaling_url: &str,
    room_code: String,
    cert_fingerprint: String,
    local_candidates: Vec<SocketAddr>,
    turn_candidate: Option<SocketAddr>,
    raw_udp: bool,
    allowed_clients: Option<Vec<String>>,
    host_name: Option<String>,
    room_ready_tx: Option<oneshot::Sender<String>>,
) -> Result<PeerMatch, SignalingError> {
    let mut ws = connect_signaling(signaling_url).await?;
    let create = SignalingMessage::CreateRoom {
        room_code: room_code.clone(),
        cert_fingerprint,
        candidates: local_candidates,
        turn_candidate,
        relay: true,
        raw_udp: Some(raw_udp),
        allowed_clients,
        host_name,
    };
    ws.send(Message::Text(serde_json::to_string(&create)?.into()))
        .await?;

    let mut ready_tx = room_ready_tx;
    loop {
        let frame = if let Some(frame) = if ready_tx.is_some() {
            timeout(SIGNAL_TIMEOUT, ws.next())
                .await
                .map_err(|_| SignalingError::TimedOut)?
        } else {
            ws.next().await
        } {
            frame
        } else {
            break;
        };
        let msg = frame?;
        let Message::Text(txt) = msg else {
            continue;
        };
        let parsed: SignalingMessage = serde_json::from_str(&txt)?;
        match parsed {
            SignalingMessage::RoomCreated { room_code: code } => {
                info!("signaling room registered");
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(code);
                }
            }
            SignalingMessage::PeerMatched {
                peer_fingerprint,
                peer_candidates,
                turn_candidate,
                relay,
                raw_udp,
                ..
            } => {
                return Ok(PeerMatch {
                    room_code,
                    peer_fingerprint,
                    peer_candidates,
                    turn_candidate,
                    relay: relay.then_some(RelayConnection(ws)),
                    raw_udp,
                });
            }
            SignalingMessage::Error { message } => {
                return Err(SignalingError::RemoteError(message));
            }
            _ => {}
        }
    }

    Err(SignalingError::Closed)
}

/// Performs Client-side WebSocket signaling: joins an existing room and retrieves host candidates + cert fingerprint.
pub async fn signal_as_client(
    signaling_url: &str,
    room_code: String,
    cert_fingerprint: String,
    mut local_candidates: Vec<SocketAddr>,
) -> Result<PeerMatch, SignalingError> {
    let mut ws = connect_signaling(signaling_url).await?;
    // The default route can be cellular while the route to the host uses the
    // tethering interface. Advertise the address proven by LAN signaling.
    if is_lan_signaling_url(signaling_url)
        && let MaybeTlsStream::Plain(tcp) = ws.get_ref()
        && let Ok(SocketAddr::V4(local)) = tcp.local_addr()
        && let Some(candidate) = local_candidates.first()
    {
        let routed = SocketAddr::new((*local.ip()).into(), candidate.port());
        if !local_candidates.contains(&routed) {
            local_candidates.insert(0, routed);
        }
    }
    let join = SignalingMessage::JoinRoom {
        room_code: room_code.clone(),
        cert_fingerprint,
        candidates: local_candidates,
        relay: true,
    };
    ws.send(Message::Text(serde_json::to_string(&join)?.into()))
        .await?;

    while let Some(frame) = timeout(SIGNAL_TIMEOUT, ws.next())
        .await
        .map_err(|_| SignalingError::TimedOut)?
    {
        let msg = frame?;
        let Message::Text(txt) = msg else {
            continue;
        };
        let parsed: SignalingMessage = serde_json::from_str(&txt)?;
        match parsed {
            SignalingMessage::PeerMatched {
                peer_fingerprint,
                peer_candidates,
                turn_candidate,
                relay,
                raw_udp,
                ..
            } => {
                return Ok(PeerMatch {
                    room_code,
                    peer_fingerprint,
                    peer_candidates,
                    turn_candidate,
                    relay: relay.then_some(RelayConnection(ws)),
                    raw_udp,
                });
            }
            SignalingMessage::Error { message } => {
                return Err(SignalingError::RemoteError(message));
            }
            _ => {}
        }
    }

    Err(SignalingError::Closed)
}

struct RoomEntry {
    id: u64,
    join: oneshot::Sender<JoiningPeer>,
    allowed_clients: Option<Vec<String>>,
    host_name: Option<String>,
}

impl RoomEntry {
    /// Joinable by a plain room code: not a saved-fingerprint room and not
    /// restricted to approved clients.
    fn publicly_joinable(&self, code: &str) -> bool {
        !code.starts_with("saved-")
            && !self
                .allowed_clients
                .as_ref()
                .is_some_and(|clients| !clients.is_empty())
    }
}

struct JoiningPeer {
    socket: ServerSocket,
    fingerprint: String,
    candidates: Vec<SocketAddr>,
    relay: bool,
}

type Rooms = Arc<Mutex<HashMap<String, RoomEntry>>>;

/// Serves private LAN rooms or public rooms behind a TLS reverse proxy.
pub struct LocalSignalingServer {
    pub local_addr: SocketAddr,
    pub ws_url: String,
    #[cfg(test)]
    discovery_addr: Option<SocketAddr>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl LocalSignalingServer {
    pub async fn start(bind_addr: &str) -> Result<Self, std::io::Error> {
        Self::start_with_discovery_port(bind_addr, LAN_DISCOVERY_PORT).await
    }

    async fn start_with_discovery_port(
        bind_addr: &str,
        discovery_port: u16,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let mut connect_addr = local_addr;
        if connect_addr.ip().is_unspecified() {
            connect_addr.set_ip(if connect_addr.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        let ws_url = format!("ws://{connect_addr}");
        let rooms = Rooms::default();
        let mailbox = Arc::new(Mutex::new(FriendMailbox::default()));
        let discovery = if local_addr.is_ipv4() && local_addr.ip().is_unspecified() {
            match UdpSocket::bind(("0.0.0.0", discovery_port)).await {
                Ok(socket) => Some(socket),
                Err(error) => {
                    debug!(%error, "LAN discovery unavailable; invite links can still be used");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        let discovery_addr = discovery
            .as_ref()
            .and_then(|socket| socket.local_addr().ok());
        let capacity = Arc::new(Semaphore::new(256));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut connections = JoinSet::new();
            if let Some(socket) = discovery {
                connections.spawn(respond_to_lan_discovery(
                    socket,
                    Arc::clone(&rooms),
                    local_addr.port(),
                ));
            }
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = connections.join_next(), if !connections.is_empty() => {}
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let Ok(permit) = Arc::clone(&capacity).try_acquire_owned() else {
                                    continue;
                                };
                                let rooms = Arc::clone(&rooms);
                                let mailbox = Arc::clone(&mailbox);
                                connections.spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = handle_relay_conn(stream, rooms, mailbox).await {
                                        debug!(%peer, %error, "signaling connection ended");
                                    }
                                });
                            }
                            Err(error) => {
                                warn!(%error, "signaling accept failed");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            ws_url,
            #[cfg(test)]
            discovery_addr,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for LocalSignalingServer {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn send_message(
    ws: &mut ServerSocket,
    message: SignalingMessage,
) -> Result<(), SignalingError> {
    timeout(
        SIGNAL_TIMEOUT,
        ws.send(Message::Text(serde_json::to_string(&message)?.into())),
    )
    .await
    .map_err(|_| SignalingError::TimedOut)??;
    Ok(())
}

async fn handle_relay_conn(
    stream: TcpStream,
    rooms: Rooms,
    mailbox: Arc<Mutex<FriendMailbox>>,
) -> Result<(), SignalingError> {
    stream
        .set_nodelay(true)
        .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(16 * 1024))
        .max_frame_size(Some(16 * 1024));
    let (mut ws, first) = timeout(SIGNAL_TIMEOUT, async {
        let mut ws = accept_async_with_config(stream, Some(config)).await?;
        let Some(Ok(Message::Text(first))) = ws.next().await else {
            return Err(SignalingError::Closed);
        };
        Ok::<_, SignalingError>((ws, first))
    })
    .await
    .map_err(|_| SignalingError::TimedOut)??;

    match serde_json::from_str(&first)? {
        SignalingMessage::FriendCommand {
            certificate,
            command,
        } => {
            let nonce = rand::random();
            send_message(&mut ws, SignalingMessage::FriendChallenge { nonce }).await?;
            let Some(Ok(Message::Text(proof))) = timeout(SIGNAL_TIMEOUT, ws.next())
                .await
                .map_err(|_| SignalingError::TimedOut)?
            else {
                return Err(SignalingError::Closed);
            };
            let SignalingMessage::FriendProof { signature } = serde_json::from_str(&proof)? else {
                return Err(SignalingError::RemoteError(
                    "Expected friend identity proof.".into(),
                ));
            };
            let certificate = rustls::pki_types::CertificateDer::from(certificate);
            let verified = webpki::EndEntityCert::try_from(&certificate).is_ok_and(|cert| {
                friend_proof_payload(&nonce, &command).is_ok_and(|payload| {
                    cert.verify_signature(webpki::ring::ECDSA_P256_SHA256, &payload, &signature)
                        .is_ok()
                })
            });
            if !verified {
                return send_message(
                    &mut ws,
                    SignalingMessage::Error {
                        message: "Friend identity verification failed.".into(),
                    },
                )
                .await;
            }
            let identity = format!("{:x}", Sha256::digest(certificate.as_ref()));
            let response = mailbox.lock().await.apply(identity, command);
            send_message(&mut ws, response).await?;
        }
        SignalingMessage::ListRooms => {
            let listing = {
                let rooms = rooms.lock().await;
                let mut summary: Vec<RoomSummary> = rooms
                    .iter()
                    .filter(|(code, entry)| entry.publicly_joinable(code))
                    .map(|(code, entry)| RoomSummary {
                        code: code.clone(),
                        name: entry.host_name.clone(),
                    })
                    .collect();
                summary.sort_by(|a, b| a.code.cmp(&b.code));
                summary.truncate(64);
                summary
            };
            send_message(&mut ws, SignalingMessage::RoomListing { rooms: listing }).await?;
        }
        SignalingMessage::CreateRoom {
            room_code,
            cert_fingerprint,
            mut candidates,
            turn_candidate,
            relay,
            raw_udp,
            allowed_clients,
            host_name,
        } => {
            let code = normalize_rendezvous(&room_code)?;
            if code.starts_with("saved-")
                && (!relay
                    || raw_udp != Some(false)
                    || !allowed_clients.as_ref().is_some_and(|clients| {
                        !clients.is_empty()
                            && clients.len() <= 64
                            && clients.iter().all(|fp| is_fingerprint(fp))
                    }))
            {
                return send_message(
                    &mut ws,
                    SignalingMessage::Error {
                        message: "saved hosting requires QUIC and an approved client".into(),
                    },
                )
                .await;
            }
            candidates.truncate(16);
            let (join, mut matched) = oneshot::channel();
            let id = rand::random();
            {
                let mut rooms = rooms.lock().await;
                if rooms.contains_key(&code) {
                    drop(rooms);
                    return send_message(
                        &mut ws,
                        SignalingMessage::Error {
                            message: "room code is already in use".to_string(),
                        },
                    )
                    .await;
                }
                rooms.insert(
                    code.clone(),
                    RoomEntry {
                        id,
                        join,
                        allowed_clients,
                        host_name,
                    },
                );
            }
            let result = async {
                send_message(
                    &mut ws,
                    SignalingMessage::RoomCreated {
                        room_code: code.clone(),
                    },
                )
                .await?;
                let mut heartbeat = interval(Duration::from_secs(20));
                loop {
                    tokio::select! {
                        peer = &mut matched => break peer.map_err(|_| SignalingError::Closed),
                        frame = ws.next() => match frame {
                            None | Some(Ok(Message::Close(_))) => break Err(SignalingError::Closed),
                            Some(Err(error)) => break Err(error.into()),
                            _ => {}
                        },
                        _ = heartbeat.tick() => {
                            timeout(SIGNAL_TIMEOUT, ws.send(Message::Ping(bytes::Bytes::new())))
                                .await.map_err(|_| SignalingError::TimedOut)??;
                        }
                    }
                }
            }
            .await;
            {
                let mut rooms = rooms.lock().await;
                if rooms.get(&code).is_some_and(|entry| entry.id == id) {
                    rooms.remove(&code);
                }
            }
            let mut peer = result?;
            let relay = relay && peer.relay;
            send_message(
                &mut ws,
                SignalingMessage::PeerMatched {
                    role: "host".to_string(),
                    peer_fingerprint: peer.fingerprint,
                    peer_candidates: peer.candidates,
                    turn_candidate: None,
                    relay,
                    raw_udp,
                },
            )
            .await?;
            send_message(
                &mut peer.socket,
                SignalingMessage::PeerMatched {
                    role: "client".to_string(),
                    peer_fingerprint: cert_fingerprint,
                    peer_candidates: candidates,
                    turn_candidate,
                    relay,
                    raw_udp,
                },
            )
            .await?;
            if relay {
                relay_pair(ws, peer.socket).await?;
            }
        }
        SignalingMessage::JoinRoom {
            room_code,
            cert_fingerprint,
            mut candidates,
            relay,
        } => {
            let code = normalize_rendezvous(&room_code)?;
            candidates.truncate(16);
            let entry = {
                let mut rooms = rooms.lock().await;
                if let Some(room) = rooms.get(&code)
                    && let Some(clients) = room.allowed_clients.as_ref()
                    && (!relay
                        || !clients
                            .iter()
                            .any(|fp| fp.eq_ignore_ascii_case(&cert_fingerprint)))
                {
                    drop(rooms);
                    return send_message(
                        &mut ws,
                        SignalingMessage::Error {
                            message: "this client has no permission to reconnect".into(),
                        },
                    )
                    .await;
                }
                rooms.remove(&code)
            };
            if let Some(entry) = entry {
                match entry.join.send(JoiningPeer {
                    socket: ws,
                    fingerprint: cert_fingerprint,
                    candidates,
                    relay,
                }) {
                    Ok(()) => return Ok(()),
                    Err(peer) => ws = peer.socket,
                }
            }
            send_message(
                &mut ws,
                SignalingMessage::Error {
                    message: "host is offline or the room was already claimed".into(),
                },
            )
            .await?;
        }
        _ => {
            return Err(SignalingError::RemoteError(
                "expected create_room or join_room".to_string(),
            ));
        }
    }
    Ok(())
}

async fn relay_pair(host: ServerSocket, client: ServerSocket) -> Result<(), SignalingError> {
    let (host_tx, host_rx) = host.split();
    let (client_tx, client_rx) = client.split();
    tokio::select! {
        result = relay_direction(host_rx, client_tx) => result,
        result = relay_direction(client_rx, host_tx) => result,
    }
}

async fn relay_direction(
    mut from: futures_util::stream::SplitStream<ServerSocket>,
    mut to: futures_util::stream::SplitSink<ServerSocket, Message>,
) -> Result<(), SignalingError> {
    while let Some(frame) = from.next().await {
        let frame = frame?;
        match &frame {
            Message::Close(_) => break,
            Message::Binary(packet) if packet.len() <= MAX_RELAY_PACKET => {}
            Message::Text(text)
                if matches!(
                    serde_json::from_str(text),
                    Ok(SignalingMessage::DirectPath { .. })
                ) => {}
            _ => continue,
        }
        timeout(SIGNAL_TIMEOUT, to.send(frame))
            .await
            .map_err(|_| SignalingError::TimedOut)??;
    }
    Ok(())
}

fn normalize_room_code(raw: &str) -> Result<String, SignalingError> {
    let mut code = raw.trim().to_ascii_uppercase();
    if code.len() == 8 && code.is_ascii() {
        code.insert(4, '-');
    }
    if code.len() == 9
        && code.as_bytes()[4] == b'-'
        && code
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_alphanumeric())
    {
        Ok(code)
    } else {
        Err(SignalingError::InvalidInvite(
            "expected an eight-character room code in XXXX-XXXX format".to_string(),
        ))
    }
}

pub fn is_fingerprint(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_rendezvous(raw: &str) -> Result<String, SignalingError> {
    if raw.strip_prefix("saved-").is_some_and(is_fingerprint) {
        Ok(raw.to_ascii_lowercase())
    } else {
        normalize_room_code(raw)
    }
}

async fn respond_to_lan_discovery(socket: UdpSocket, rooms: Rooms, port: u16) {
    let lan_ip = crate::stun::discover_primary_lan_ip();
    let lan_endpoint = Some(format!(
        "ws://{}",
        SocketAddr::new(lan_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST.into()), port,)
    ));
    let host_name = computer_display_name();
    let mut packet = [0; 512];
    loop {
        let (len, source) = match socket.recv_from(&mut packet).await {
            Ok(received) => received,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(_) => break,
        };
        if len == 16 && &packet[..8] == PRESENCE_QUERY {
            let reply =
                presence_reply(&rooms, &lan_endpoint, host_name.as_deref(), &packet[8..16]).await;
            if let Some(reply) = reply {
                let _ = socket.send_to(&reply, source).await;
            }
            continue;
        }
        if len != 34 || &packet[..8] != DISCOVERY_QUERY {
            continue;
        }
        let nonce = &packet[8..16];
        let proof = rooms.lock().await.keys().find_map(|code| {
            (discovery_proof(DISCOVERY_QUERY, nonce, code, 0) == packet[16..32])
                .then(|| discovery_proof(DISCOVERY_REPLY, nonce, code, port))
        });
        if let Some(proof) = proof {
            let mut reply = [0; 34];
            reply[..8].copy_from_slice(DISCOVERY_REPLY);
            reply[8..16].copy_from_slice(nonce);
            reply[16..18].copy_from_slice(&port.to_be_bytes());
            reply[18..].copy_from_slice(&proof);
            let _ = socket.send_to(&reply, source).await;
        }
    }
}

/// Builds the browse reply for a presence query: magic + nonce echo + room code
/// + LAN signaling endpoint + optional host display name. Only answers while a
/// joinable room is registered, so idle hosts stay invisible.
async fn presence_reply(
    rooms: &Rooms,
    lan_endpoint: &Option<String>,
    host_name: Option<&str>,
    nonce: &[u8],
) -> Option<Vec<u8>> {
    if lan_endpoint.is_none() {
        return None;
    }
    let rooms = rooms.lock().await;
    let code = rooms
        .iter()
        .find_map(|(code, entry)| entry.publicly_joinable(code).then(|| code.clone()))?;
    drop(rooms);
    let endpoint = lan_endpoint.as_deref().unwrap_or_default();
    let name = host_name.unwrap_or_default();
    let mut reply = Vec::with_capacity(8 + 8 + code.len() + 2 + endpoint.len() + 2 + name.len());
    reply.extend_from_slice(PRESENCE_REPLY);
    reply.extend_from_slice(nonce);
    reply.extend_from_slice(code.as_bytes());
    reply.extend_from_slice(&(endpoint.len() as u16).to_be_bytes());
    reply.extend_from_slice(endpoint.as_bytes());
    reply.extend_from_slice(&(name.len() as u16).to_be_bytes());
    reply.extend_from_slice(name.as_bytes());
    Some(reply)
}

/// The machine name advertised to peers during discovery. Sourced from the
/// operating system, trimmed, and capped so broadcast replies stay small.
pub fn computer_display_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    let raw = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(target_os = "windows"))]
    let raw = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok());
    raw.map(|name| {
        let name = name.trim();
        let name = if name.is_empty() { "VmaX host" } else { name };
        name.chars().take(64).collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredHost {
    pub code: String,
    pub endpoint: String,
    pub name: Option<String>,
    pub addr: String,
}

/// Broadcasts a presence query on the local network and collects hosting peers.
/// Hosts answer only while they hold a joinable room, and replies arrive as
/// unicast, which keeps this working on Android without a multicast lock.
pub async fn discover_lan_hosts(scan: Duration) -> Result<Vec<DiscoveredHost>, SignalingError> {
    let socket =
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|error| SignalingError::EndpointIo {
                endpoint: "local network scan".into(),
                stage: "UDP socket setup",
                source: error,
            })?;
    socket
        .set_broadcast(true)
        .map_err(|error| SignalingError::EndpointIo {
            endpoint: "local network scan".into(),
            stage: "UDP socket setup",
            source: error,
        })?;
    let mut request = [0; 16];
    request[..8].copy_from_slice(PRESENCE_QUERY);
    let nonce = rand::random::<u64>().to_be_bytes();
    request[8..16].copy_from_slice(&nonce);
    let mut reply = [0; 512];
    let mut tick = interval(Duration::from_millis(250));
    let deadline = Instant::now() + scan.clamp(Duration::from_millis(300), Duration::from_secs(5));
    let mut found: Vec<DiscoveredHost> = Vec::new();
    let targets = lan_discovery_targets();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if Instant::now() >= deadline {
                    break;
                }
                send_lan_discovery(&socket, &request, &targets, LAN_DISCOVERY_PORT).await;
            }
            received = socket.recv_from(&mut reply) => {
                if let Ok((len, peer)) = received
                    && peer.port() == LAN_DISCOVERY_PORT
                    && len >= 25
                    && &reply[..8] == PRESENCE_REPLY
                    && &reply[8..16] == nonce
                {
                    if let Some(mut host) = parse_presence_reply(&reply[..len])
                        && let Some(port) = Url::parse(&host.endpoint).ok().and_then(|url| url.port_or_known_default())
                        && port != 0
                    {
                        host.endpoint = format!("ws://{}", SocketAddr::new(peer.ip(), port));
                        if !found.iter().any(|known| known.code == host.code && known.endpoint == host.endpoint) {
                            found.push(DiscoveredHost { addr: peer.ip().to_string(), ..host });
                        }
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.code.cmp(&b.code));
    Ok(found)
}

fn parse_presence_reply(packet: &[u8]) -> Option<DiscoveredHost> {
    let code = std::str::from_utf8(&packet[16..25]).ok()?;
    normalize_room_code(code).ok()?;
    let mut cursor = 25;
    let take_string = |packet: &[u8], cursor: &mut usize| -> Option<String> {
        if *cursor + 2 > packet.len() {
            return None;
        }
        let length = u16::from_be_bytes([packet[*cursor], packet[*cursor + 1]]) as usize;
        *cursor += 2;
        let end = *cursor + length;
        if end > packet.len() {
            return None;
        }
        let text = std::str::from_utf8(&packet[*cursor..end]).ok()?;
        *cursor = end;
        Some(text.to_owned())
    };
    let endpoint = take_string(packet, &mut cursor)?;
    if !endpoint.starts_with("ws://") {
        return None;
    }
    let name = if cursor + 2 <= packet.len() {
        match take_string(packet, &mut cursor) {
            Some(name) if name.is_empty() => None,
            Some(name) => Some(name),
            None => None,
        }
    } else {
        None
    };
    Some(DiscoveredHost {
        code: code.to_owned(),
        endpoint,
        name,
        addr: String::new(),
    })
}

/// Asks a signaling endpoint for its currently joinable rooms. Works with the
/// built-in LAN relay and with public `wss://` relays deployed from this repo.
pub async fn list_relay_rooms(signaling_url: &str) -> Result<Vec<RoomSummary>, SignalingError> {
    let mut ws = connect_signaling(signaling_url).await?;
    timeout(
        SIGNAL_TIMEOUT,
        ws.send(Message::Text(
            serde_json::to_string(&SignalingMessage::ListRooms)?.into(),
        )),
    )
    .await
    .map_err(|_| SignalingError::TimedOut)??;
    loop {
        let frame = timeout(SIGNAL_TIMEOUT, ws.next())
            .await
            .map_err(|_| SignalingError::TimedOut)?
            .ok_or(SignalingError::Closed)?;
        let Message::Text(text) = frame? else {
            continue;
        };
        let parsed: SignalingMessage = serde_json::from_str(&text)?;
        match parsed {
            SignalingMessage::RoomListing { rooms } => return Ok(rooms),
            SignalingMessage::Error { message } => {
                return Err(SignalingError::RemoteError(message));
            }
            _ => {}
        }
    }
}

pub async fn discover_lan_signaling(room_code: &str) -> Option<String> {
    let code = normalize_rendezvous(room_code).ok()?;
    let mut probes = JoinSet::new();
    probes.spawn(discover_room_on(
        code.clone(),
        std::net::Ipv4Addr::UNSPECIFIED,
        lan_discovery_targets(),
        LAN_DISCOVERY_PORT,
    ));
    #[cfg(target_os = "android")]
    for (address, mask) in android_lan_interfaces() {
        probes.spawn(discover_room_on(
            code.clone(),
            address.into(),
            vec![(address | !mask).into()],
            LAN_DISCOVERY_PORT,
        ));
    }
    while let Some(result) = probes.join_next().await {
        if let Ok(Some(endpoint)) = result {
            return Some(endpoint);
        }
    }
    None
}

async fn discover_room_on(
    code: String,
    source: std::net::Ipv4Addr,
    targets: Vec<std::net::Ipv4Addr>,
    discovery_port: u16,
) -> Option<String> {
    let socket = UdpSocket::bind((source, 0)).await.ok()?;
    socket.set_broadcast(true).ok()?;
    let mut request = [0; 34];
    request[..8].copy_from_slice(DISCOVERY_QUERY);
    request[8..16].copy_from_slice(&rand::random::<u64>().to_be_bytes());
    let proof = discovery_proof(DISCOVERY_QUERY, &request[8..16], &code, 0);
    request[16..32].copy_from_slice(&proof);
    let mut reply = [0; 64];
    let mut tick = interval(Duration::from_millis(200));

    timeout(Duration::from_millis(850), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    send_lan_discovery(&socket, &request, &targets, discovery_port).await;
                }
                received = socket.recv_from(&mut reply) => {
                    if let Ok((34, peer)) = received
                        && peer.port() == discovery_port
                        && &reply[..8] == DISCOVERY_REPLY
                        && reply[8..16] == request[8..16]
                    {
                        let port = u16::from_be_bytes([reply[16], reply[17]]);
                        if port != 0 && reply[18..34] == discovery_proof(DISCOVERY_REPLY, &request[8..16], &code, port) {
                            return format!("ws://{}", SocketAddr::new(peer.ip(), port));
                        }
                    }
                }
            }
        }
    }).await.ok()
}

fn lan_discovery_targets() -> Vec<std::net::Ipv4Addr> {
    use std::net::Ipv4Addr;
    let targets = vec![Ipv4Addr::BROADCAST, Ipv4Addr::LOCALHOST];
    #[cfg(target_os = "android")]
    let targets = {
        let mut targets = targets;
        for (address, mask) in android_lan_interfaces() {
            let broadcast = Ipv4Addr::from(address | !mask);
            if !targets.contains(&broadcast) {
                targets.push(broadcast);
            }
        }
        targets
    };
    targets
}

#[cfg(target_os = "android")]
pub(crate) fn android_lan_source(peer: std::net::Ipv4Addr) -> Option<std::net::Ipv4Addr> {
    let peer = u32::from(peer);
    android_lan_interfaces()
        .into_iter()
        .filter(|(address, mask)| address & mask == peer & mask)
        .max_by_key(|(_, mask)| mask.count_ones())
        .map(|(address, _)| address.into())
}

#[cfg(target_os = "android")]
fn android_lan_interfaces() -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    let mut interfaces = std::ptr::null_mut();
    // getifaddrs owns this linked list until freeifaddrs. Check each address
    // family before reading the corresponding sockaddr_in.
    unsafe {
        if libc::getifaddrs(&mut interfaces) != 0 {
            return result;
        }
        let mut current = interfaces;
        while let Some(interface) = current.as_ref() {
            current = interface.ifa_next;
            let required = (libc::IFF_UP | libc::IFF_BROADCAST) as libc::c_uint;
            if interface.ifa_flags & required != required {
                continue;
            }
            let (Some(addr), Some(mask)) =
                (interface.ifa_addr.as_ref(), interface.ifa_netmask.as_ref())
            else {
                continue;
            };
            if addr.sa_family as i32 != libc::AF_INET || mask.sa_family as i32 != libc::AF_INET {
                continue;
            }
            let address = (*interface.ifa_addr.cast::<libc::sockaddr_in>())
                .sin_addr
                .s_addr;
            let mask = (*interface.ifa_netmask.cast::<libc::sockaddr_in>())
                .sin_addr
                .s_addr;
            if mask != 0 && mask != u32::MAX {
                result.push((u32::from_be(address), u32::from_be(mask)));
            }
        }
        libc::freeifaddrs(interfaces);
    }
    result
}

async fn send_lan_discovery(
    socket: &UdpSocket,
    request: &[u8],
    targets: &[std::net::Ipv4Addr],
    port: u16,
) {
    for target in targets {
        let _ = socket.send_to(request, (*target, port)).await;
    }
}

fn discovery_proof(domain: &[u8], nonce: &[u8], code: &str, port: u16) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(nonce);
    hash.update(code.as_bytes());
    hash.update(port.to_be_bytes());
    let mut proof = [0; 16];
    proof.copy_from_slice(&hash.finalize()[..16]);
    proof
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn unreachable_first_address_does_not_block_working_alternative() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unreachable = "192.0.2.1:9".parse().unwrap();
        let socket = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::connect_candidates(&[unreachable, listener.local_addr().unwrap()]),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(socket.peer_addr().unwrap(), listener.local_addr().unwrap());
    }
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn invite_uri_roundtrip() {
        let code = "AB12-CD34";
        let sig = "ws://192.168.1.50:8787";
        let fp = "deadbeef0123456789";
        let uri = build_invite_uri(code, sig, fp, false).unwrap();
        assert!(uri.starts_with("vmax://join/AB12-CD34"));

        let parsed = parse_invite_input(&uri, "ws://127.0.0.1:8787").unwrap();
        assert_eq!(parsed.room_code, "AB12-CD34");
        assert_eq!(parsed.signaling_url, sig);
        assert_eq!(parsed.expected_host_fingerprint.as_deref(), Some(fp));
        assert_eq!(parsed.raw_udp, Some(false));
        assert!(!parsed.discover_lan);
        assert!(parsed.endpoint_from_link);
    }

    #[test]
    fn full_invite_keeps_its_endpoint_while_a_short_code_can_discover_lan() {
        let endpoint = "ws://127.0.0.1:9010";
        let uri = build_invite_uri("ABCD-5678", endpoint, "test-pin", false).unwrap();
        let invite = parse_invite_input(&uri, "ws://127.0.0.1:8787").unwrap();
        assert_eq!(invite.signaling_url, endpoint);
        assert!(!invite.discover_lan);

        let capitalized = uri.replacen("vmax://join", "Vmax://JOIN", 1);
        let invite = parse_invite_input(&capitalized, "ws://127.0.0.1:8787").unwrap();
        assert_eq!(invite.room_code, "ABCD-5678");
        assert_eq!(invite.signaling_url, endpoint);

        let short_code = parse_invite_input("ABCD-5678", "ws://127.0.0.1:8787").unwrap();
        assert!(short_code.discover_lan);
        assert!(!short_code.endpoint_from_link);

        let remote_code = parse_invite_input("ABCD-5678", "wss://relay.example.com").unwrap();
        assert!(!remote_code.discover_lan);
    }

    #[tokio::test]
    async fn reports_refused_signaling_port_without_exposing_the_url_query() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = socket.local_addr().unwrap();
        let error = connect_signaling(&format!("ws://{address}/stream?token=private"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, SignalingError::EndpointRefused { endpoint, .. } if endpoint == &format!("ws://{address}"))
        );
        assert!(!error.to_string().contains("private"));
    }

    #[tokio::test]
    async fn embedded_signaling_matches_host_and_client() {
        let mut server = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        let url = server.ws_url.clone();
        let (ready_tx, ready_rx) = oneshot::channel();

        let host_url = url.clone();
        let host_task = tokio::spawn(async move {
            signal_as_host(
                &host_url,
                "TEST-9999".to_string(),
                "host-fp".to_string(),
                vec!["127.0.0.1:50001".parse().unwrap()],
                Some("192.0.2.20:50003".parse().unwrap()),
                false,
                None,
                Some("Test bench".to_string()),
                Some(ready_tx),
            )
            .await
            .unwrap()
        });

        let registered = ready_rx.await.unwrap();
        assert_eq!(registered, "TEST-9999");

        let client_match = signal_as_client(
            &url,
            "TEST-9999".to_string(),
            "client-fp".to_string(),
            vec!["10.20.30.40:50002".parse().unwrap()],
        )
        .await
        .unwrap();

        let host_match = host_task.await.unwrap();
        assert_eq!(client_match.turn_candidate, Some("192.0.2.20:50003".parse().unwrap()));
        assert_eq!(host_match.turn_candidate, None);
        assert_eq!(client_match.peer_fingerprint, "host-fp");
        assert_eq!(host_match.peer_fingerprint, "client-fp");
        assert_eq!(
            host_match.peer_candidates.first(),
            Some(&"127.0.0.1:50002".parse().unwrap())
        );
        server.stop();
    }

    #[tokio::test]
    async fn short_code_discovers_a_registered_lan_room() {
        let server = LocalSignalingServer::start_with_discovery_port("0.0.0.0:0", 0)
            .await
            .unwrap();
        let discovery_port = server.discovery_addr.unwrap().port();
        let url = server.ws_url.clone();
        let room_code = generate_room_code();
        let host_room = room_code.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let host = tokio::spawn(async move {
            signal_as_host(
                &url,
                host_room,
                "host-fp".into(),
                Vec::new(),
                None,
                true,
                None,
                None,
                Some(ready_tx),
            )
            .await
        });
        ready_rx.await.unwrap();
        let short_code = room_code.to_ascii_lowercase().replace('-', "");
        let probe = |code: &str| {
            let code = normalize_rendezvous(code).unwrap();
            discover_room_on(
                code,
                Ipv4Addr::LOCALHOST,
                vec![Ipv4Addr::LOCALHOST],
                discovery_port,
            )
        };
        let discovered = probe(&short_code).await.unwrap();
        assert_eq!(
            Url::parse(&discovered).unwrap().port(),
            Some(server.local_addr.port())
        );
        assert!(probe("WXYZ-9999").await.is_none());
        let client = signal_as_client(&discovered, room_code, "client-fp".into(), Vec::new())
            .await
            .unwrap();
        assert_eq!(client.raw_udp, Some(true));
        assert_eq!(host.await.unwrap().unwrap().peer_fingerprint, "client-fp");
    }

    #[tokio::test]
    async fn presence_reply_roundtrips_through_the_client_parser() {
        let rooms = Rooms::default();
        let endpoint = Some("ws://192.168.1.50:8787".to_string());
        let nonce = [7_u8; 8];
        let reply = presence_reply(&rooms, &endpoint, Some("Desk-PC"), &nonce).await;
        assert!(reply.is_none(), "no rooms registered yet");

        let entry = RoomEntry {
            id: 1,
            join: oneshot::channel().0,
            allowed_clients: None,
            host_name: Some("Desk-PC".into()),
        };
        rooms.lock().await.insert("ABCD-5678".to_string(), entry);
        let reply = presence_reply(&rooms, &endpoint, Some("Desk-PC"), &nonce)
            .await
            .expect("joinable room answers presence");
        assert_eq!(&reply[..8], PRESENCE_REPLY);
        assert_eq!(&reply[8..16], &nonce);
        let host = parse_presence_reply(&reply).expect("parser accepts own reply");
        assert_eq!(host.code, "ABCD-5678");
        assert_eq!(host.endpoint, "ws://192.168.1.50:8787");
        assert_eq!(host.name.as_deref(), Some("Desk-PC"));
    }

    #[tokio::test]
    async fn relay_lists_joinable_rooms_with_host_names() {
        let mut server = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        let url = server.ws_url.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let host = tokio::spawn(async move {
            signal_as_host(
                &url,
                "LIST-0001".into(),
                "host-fp".into(),
                Vec::new(),
                None,
                false,
                None,
                Some("Gaming Rig".into()),
                Some(ready_tx),
            )
            .await
        });
        ready_rx.await.unwrap();

        let rooms = list_relay_rooms(&server.ws_url).await.unwrap();
        assert_eq!(
            rooms,
            vec![RoomSummary {
                code: "LIST-0001".into(),
                name: Some("Gaming Rig".into())
            }]
        );

        // Saved-fingerprint rooms stay hidden from listings.
        let (saved_tx, saved_rx) = oneshot::channel();
        let saved_url = server.ws_url.clone();
        let saved_host = tokio::spawn(async move {
            let _ = signal_as_host(
                &saved_url,
                "saved-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                "host-fp".into(),
                Vec::new(),
                None,
                false,
                Some(vec!["b".repeat(64)]),
                None,
                Some(saved_tx),
            )
            .await;
        });
        saved_rx.await.unwrap();
        let rooms = list_relay_rooms(&server.ws_url).await.unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].code, "LIST-0001");
        host.abort();
        saved_host.abort();
        server.stop();
    }
}
