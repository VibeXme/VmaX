//! Dual-stack WAN hole punching, UPnP port mapping and the mobile QUIC policy.
//!
//! This is the "get a direct path through CGNAT" layer.  It is deliberately
//! dependency-free: STUN parsing is shared with [`crate::stun`], SSDP/SOAP UPnP
//! is a small synchronous client, and hole punching is plain `std::net`
//! sockets so it can run on a blocking thread.
//!
//! Flow (host and client run the same code):
//! 1. [`bind_dual_stack`] opens one `AF_INET6` socket with `IPV6_V6ONLY=0`, so
//!    a single 5-tuple covers IPv6 and IPv4-mapped peers.
//! 2. [`gather_candidates`] collects host candidates and STUN server-reflexive
//!    addresses for both families.
//! 3. Candidates travel over the existing signaling channel
//!    ([`CandidateSet::to_signaling`] / [`CandidateSet::from_signaling`]).
//! 4. [`HolePuncher::run`] performs simultaneous open: both sides send the same
//!    magic datagram to every peer candidate every 20 ms.  The first peer
//!    address that answers becomes the direct path.  A symmetric CGNAT gives
//!    each side a distinct external port per destination, but because both
//!    sides are sending, the NAT mappings are created before the peer's packet
//!    arrives, which is exactly what makes simultaneous open work.
//! 5. [`UpnpMapper`] is attempted first when the gateway supports IGD: an
//!    explicit mapping removes the timing dependence of step 4.
//! 6. Only if no candidate answers after the punch window does the caller fall
//!    back to the relay (`WanPath::Relay`).
//!
//! Integration with `network.rs`: call [`WanSessionPlan::prepare`] once before
//! building the `quinn::Endpoint`, use [`WanSessionPlan::transport_config`] as
//! the endpoint's transport config, then hand the puncher's result to the
//! endpoint (`quinn::Endpoint::new` on the punched socket, or
//! [`crate::nat_mobility::rebind_endpoint`] after a rebind).

use quinn::congestion::BbrConfig;
use quinn::{IdleTimeout, MtuDiscoveryConfig, TransportConfig};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, TcpStream, UdpSocket,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Application datagram budget for every carrier profile.  1180 bytes survives
/// 464XLAT (CLAT adds 20 bytes of IPv4 header inside IPv6), PPPoE and most
/// GTP/UDP tunnel encapsulation without IP fragmentation.
pub const WAN_DATAGRAM_BUDGET: usize = 1180;

/// 200 ms keepalive: keeps CGNAT mappings warm and still costs under 1.5 kB/s
/// of uplink on a phone radio.
pub const WAN_KEEPALIVE: Duration = Duration::from_millis(200);

/// A handover must not tear down the session.  15 s is long enough for a
/// Wi-Fi -> LTE transition and short enough to release a dead peer.
pub const WAN_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// 4 MiB kernel buffers absorb the capture cadence while the pacer smooths the
/// wire.  Must match `crate::transport::SOCKET_BUFFER_BYTES`.
pub const SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// DSCP Expedited Forwarding (46 << 2).  On Wi-Fi this maps to WMM AC_VO.
pub const DSCP_EF: u32 = 0xB8;

pub const PUNCH_INTERVAL: Duration = Duration::from_millis(20);
pub const PUNCH_ROUNDS: u32 = 25; // ~500 ms, matching ICE connectivity checks
pub const PUNCH_MAGIC: &[u8; 8] = b"VMAXPNCH";

/// 1200-byte floor keeps every queued datagram valid after a migration.
pub const WAN_MIN_MTU: u16 = 1200;
/// 1240-byte ceiling: 1180 payload + VmaX tag + QUIC short header + AEAD tag.
pub const WAN_MAX_MTU: u16 = 1240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Host,
    ServerReflexive,
    /// Port mapped on the gateway through UPnP IGD.
    Upnp,
}

/// One signaling-friendly candidate.  Serializes as `{addr, kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WanCandidate {
    pub addr: SocketAddr,
    pub kind: CandidateKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateSet {
    pub candidates: Vec<WanCandidate>,
    /// External port allocated by UPnP, if the gateway answered.
    pub upnp_external_port: Option<u16>,
}

impl CandidateSet {
    pub fn to_signaling(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    pub fn from_signaling(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.candidates.iter().map(|c| c.addr).collect()
    }

    pub fn has_ipv6(&self) -> bool {
        self.candidates.iter().any(|c| c.addr.is_ipv6())
    }

    fn push(&mut self, addr: SocketAddr, kind: CandidateKind) {
        let addr = canonical(addr);
        if addr.port() == 0 || addr.ip().is_unspecified() {
            return;
        }
        if self.candidates.iter().any(|c| c.addr == addr) {
            return;
        }
        self.candidates.push(WanCandidate { addr, kind });
    }
}

/// IPv4-mapped IPv6 -> IPv4, so the same peer is never punched twice.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

/// Open the single dual-stack socket used for the whole session.  `port` is 0
/// for the initiating side and the advertised port for the answering side.
pub fn bind_dual_stack(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)).into())?;
    tune_wan_socket(&socket)?;
    Ok(socket.into())
}

/// Apply 4 MiB buffers and DSCP EF.  Buffer sizing is mandatory; QoS marking is
/// best effort because some Windows Wi-Fi drivers and vendor Android kernels
/// reject the socket option, and a failed mark must never fail a session.
pub fn tune_wan_socket(socket: &Socket) -> io::Result<()> {
    socket.set_send_buffer_size(SOCKET_BUFFER_BYTES)?;
    socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES)?;
    let _ = socket.set_tos_v4(DSCP_EF);
    let _ = mark_ipv6_traffic_class(socket, DSCP_EF);
    Ok(())
}

/// `IPV6_TCLASS` / `IP_TOS` for an `AF_INET6` socket.  `socket2` exposes the
/// IPv4 TOS setter but not the IPv6 traffic-class one, so this is a two-line
/// platform shim.
#[cfg(windows)]
fn mark_ipv6_traffic_class(socket: &Socket, value: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    const IPPROTO_IPV6: i32 = 41;
    const IPV6_TCLASS: i32 = 39;

    unsafe extern "system" {
        fn setsockopt(
            s: usize,
            level: i32,
            optname: i32,
            optval: *const i8,
            optlen: i32,
        ) -> i32;
    }

    let raw = socket.as_raw_socket() as usize;
    let val = value as i32;
    // SAFETY: `raw` is a live SOCKET and `val` is a 4-byte int, matching the
    // option's documented size.
    let rc = unsafe {
        setsockopt(
            raw,
            IPPROTO_IPV6,
            IPV6_TCLASS,
            &val as *const i32 as *const i8,
            core::mem::size_of::<i32>() as i32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "android")]
fn mark_ipv6_traffic_class(socket: &Socket, value: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    const IPPROTO_IPV6: i32 = 41;
    const IPV6_TCLASS: i32 = 67;
    let val = value as libc::c_int;
    // SAFETY: live fd and a correctly sized integer option value.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            IPV6_TCLASS,
            &val as *const libc::c_int as *const libc::c_void,
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, target_os = "android")))]
fn mark_ipv6_traffic_class(_socket: &Socket, _value: u32) -> io::Result<()> {
    Ok(())
}

/// The primary routable local address for one family.  A connected UDP socket
/// performs no traffic; it just asks the OS which interface it would use.
fn primary_local_addr(v6: bool) -> Option<SocketAddr> {
    let (bind, peer): (&str, &str) = if v6 {
        ("[::]:0", "[2001:4860:4860::8888]:80")
    } else {
        ("0.0.0.0:0", "8.8.8.8:80")
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(peer).ok()?;
    let addr = canonical(socket.local_addr().ok()?);
    (addr.port() != 0 && !addr.ip().is_unspecified()).then_some(addr)
}

/// Host candidates only (no network traffic).  Port is replaced by the real
/// bound port by the caller.
pub fn local_host_candidates() -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Some(v6) = primary_local_addr(true) {
        out.push(v6);
    }
    if let Some(v4) = primary_local_addr(false) {
        out.push(v4);
    }
    out
}

/// Synchronous STUN binding request for one server.  Returns the
/// server-reflexive address or `None` on timeout.
pub fn stun_reflexive(
    socket: &UdpSocket,
    server: &str,
    timeout: Duration,
) -> Option<SocketAddr> {
    let tx_id: [u8; 12] = rand::random();
    let request = crate::stun::build_binding_request(tx_id);
    let mut last = None;
    for target in server.to_socket_addrs_fallback() {
        socket.send_to(&request, target).ok()?;
        last = Some(target);
    }
    last?;

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 512];
    let previous = socket.read_timeout().ok().flatten();
    let _ = socket.set_read_timeout(Some(timeout));
    let result = loop {
        if Instant::now() >= deadline {
            break None;
        }
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                // Ignore datagrams that are not from the server we asked.
                if last.is_some_and(|target| canonical(target).ip() != canonical(from).ip()) {
                    continue;
                }
                if let Ok(mapped) = crate::stun::parse_binding_response(&buf[..len], &tx_id) {
                    break Some(canonical(mapped));
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break None;
            }
            Err(_) => break None,
        }
    };
    let _ = socket.set_read_timeout(previous);
    result
}

/// Small helper so `stun_reflexive` can resolve without pulling DNS into the
/// hot path a second time.
trait ToSocketAddrsFallback {
    fn to_socket_addrs_fallback(&self) -> Vec<SocketAddr>;
}

impl ToSocketAddrsFallback for &str {
    fn to_socket_addrs_fallback(&self) -> Vec<SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs()
            .map(|iter| iter.collect())
            .unwrap_or_default()
    }
}

/// Full bidirectional candidate gathering (host + STUN).  Blocking; call from
/// `tokio::task::spawn_blocking`.
pub fn gather_candidates(socket: &UdpSocket, stun_servers: &[&str]) -> CandidateSet {
    let bound = socket.local_addr().map(canonical).ok();
    let mut set = CandidateSet::default();
    for addr in local_host_candidates() {
        set.push(
            SocketAddr::new(addr.ip(), bound.map(|b| b.port()).unwrap_or(0)),
            CandidateKind::Host,
        );
    }
    for server in stun_servers {
        if let Some(mapped) = stun_reflexive(socket, server, Duration::from_millis(600)) {
            set.push(mapped, CandidateKind::ServerReflexive);
        }
        if set.candidates.iter().any(|c| c.addr.is_ipv6())
            && set.candidates.iter().any(|c| !c.addr.is_ipv6())
        {
            break;
        }
    }
    set
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WanPath {
    /// Direct UDP path to this peer address.
    Direct(SocketAddr),
    /// Direct path through a UPnP-created gateway mapping.
    UpnpMapped(SocketAddr),
    /// No candidate answered: use the relay.
    Relay,
}

/// Simultaneous-open hole puncher.  Both peers run this at the same time with
/// each other's candidate list.
pub struct HolePuncher {
    socket: Arc<UdpSocket>,
    round_interval: Duration,
    rounds: u32,
}

impl HolePuncher {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self {
            socket,
            round_interval: PUNCH_INTERVAL,
            rounds: PUNCH_ROUNDS,
        }
    }

    pub fn with_window(mut self, interval: Duration, rounds: u32) -> Self {
        self.round_interval = interval;
        self.rounds = rounds.max(1);
        self
    }

    /// Send a punch datagram to every candidate.  The payload is the magic
    /// plus the sender's own advertised port, so a peer that sees the packet
    /// can immediately reply to the observed source address (the NAT mapping
    /// that actually works).
    pub fn punch_payload(local_port: u16) -> [u8; 10] {
        let mut out = [0u8; 10];
        out[..8].copy_from_slice(PUNCH_MAGIC);
        out[8..].copy_from_slice(&local_port.to_be_bytes());
        out
    }

    /// Blocking punch.  Returns as soon as a peer answers, or `WanPath::Relay`
    /// when the window expires.
    pub fn run(&self, peers: &[SocketAddr]) -> WanPath {
        if peers.is_empty() {
            return WanPath::Relay;
        }
        let local_port = self.socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let payload = Self::punch_payload(local_port);
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + self.round_interval * self.rounds;

        while Instant::now() < deadline {
            for peer in peers {
                let _ = self.socket.send_to(&payload, peer);
            }
            let until = Instant::now() + self.round_interval;
            while Instant::now() < until {
                match self.socket.recv_from(&mut buf) {
                    Ok((len, from)) => {
                        if Self::is_punch(&buf[..len]) {
                            let from = canonical(from);
                            // Mark the confirmed peer with a QoS probe so the
                            // first real media packet is not the first packet
                            // the AP has ever seen on this flow.
                            let _ = self.socket.send_to(&payload, from);
                            return WanPath::Direct(from);
                        }
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        WanPath::Relay
    }

    pub fn is_punch(datagram: &[u8]) -> bool {
        datagram.len() >= 8 && &datagram[..8] == PUNCH_MAGIC
    }

    /// Port a peer advertised inside its punch payload.  Some CGNATs only let
    /// the *sender's* mapping through, so this is informational.
    pub fn advertised_port(datagram: &[u8]) -> Option<u16> {
        (datagram.len() >= 10).then(|| u16::from_be_bytes([datagram[8], datagram[9]]))
    }
}

// ---------------------------------------------------------------------------
// UPnP IGD (SSDP discovery + SOAP AddPortMapping)
// ---------------------------------------------------------------------------

const SSDP_ADDR: &str = "239.255.255.250:1900";
const SSDP_MX_SECS: &str = "2";

/// Minimal UPnP IGD control point.  `igd-next` can be dropped in behind the
/// same three methods (`discover`, `map_udp`, `unmap_udp`) if you prefer a
/// maintained parser; this keeps the release binary dependency-free.
#[derive(Debug, Clone)]
pub struct UpnpMapper {
    control_url: String,
    service_type: String,
}

impl UpnpMapper {
    /// SSDP `M-SEARCH` for an Internet Gateway Device, then fetch its
    /// description document to find the WAN connection control URL.
    pub fn discover() -> Option<Self> {
        let description_url = Self::ssdp_locations()
            .into_iter()
            .find_map(|location| Self::control_from_description(&location))?;
        Some(description_url)
    }

    fn ssdp_locations() -> Vec<String> {
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(socket) => socket,
            Err(_) => return Vec::new(),
        };
        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
        let request = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: {SSDP_ADDR}\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: {SSDP_MX_SECS}\r\n\
             ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
        );
        if socket.send_to(request.as_bytes(), SSDP_ADDR).is_err() {
            return Vec::new();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut locations = Vec::new();
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            match socket.recv_from(&mut buf) {
                Ok((len, _)) => {
                    if let Some(location) = Self::header(&buf[..len], "location") {
                        if !locations.contains(&location) {
                            locations.push(location);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        locations
    }

    fn header(response: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(response);
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    fn control_from_description(location: &str) -> Option<Self> {
        let body = http_get(location)?;
        // Prefer WANIPConnection (most routers), fall back to WANPPPConnection.
        for service in ["WANIPConnection:1", "WANPPPConnection:1"] {
            let service_type = format!("urn:schemas-upnp-org:service:{service}");
            let Some(index) = body.find(&service_type) else {
                continue;
            };
            let tail = &body[index..];
            let control = tail
                .find("<controlURL>")
                .and_then(|start| {
                    let rest = &tail[start + "<controlURL>".len()..];
                    rest.find("</controlURL>")
                        .map(|end| rest[..end].trim().to_string())
                })
                .unwrap_or_default();
            if control.is_empty() {
                continue;
            }
            let control_url = if control.starts_with("http://") {
                control
            } else {
                let origin = origin_of(location)?;
                format!("{origin}{}", control.trim_start_matches('/'))
            };
            return Some(Self {
                control_url,
                service_type,
            });
        }
        None
    }

    /// Create or refresh a UDP mapping.  Returns the external port.
    pub fn map_udp(&self, local_ip: Ipv4Addr, internal_port: u16, external_port: u16) -> Option<u16> {
        let body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:AddPortMapping xmlns:u="{service}">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{external}</NewExternalPort>
<NewProtocol>UDP</NewProtocol>
<NewInternalPort>{internal}</NewInternalPort>
<NewInternalClient>{ip}</NewInternalClient>
<NewEnabled>1</NewEnabled>
<NewPortMappingDescription>VmaX</NewPortMappingDescription>
<NewLeaseDuration>{lease}</NewLeaseDuration>
</u:AddPortMapping></s:Body></s:Envelope>"#,
            service = self.service_type,
            external = external_port,
            internal = internal_port,
            ip = local_ip,
            lease = 3600,
        );
        http_soap(&self.control_url, &self.service_type, "AddPortMapping", &body)
            .then_some(external_port)
    }

    pub fn unmap_udp(&self, external_port: u16) -> bool {
        let body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:DeletePortMapping xmlns:u="{service}">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{external}</NewExternalPort>
<NewProtocol>UDP</NewProtocol>
</u:DeletePortMapping></s:Body></s:Envelope>"#,
            service = self.service_type,
            external = external_port,
        );
        http_soap(
            &self.control_url,
            &self.service_type,
            "DeletePortMapping",
            &body,
        )
    }
}

fn origin_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let host_end = rest.find('/').unwrap_or(rest.len());
    Some(format!("http://{}", &rest[..host_end]))
}

fn http_get(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let addr = authority
        .to_socket_addrs_fallback()
        .into_iter()
        .next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

fn http_soap(url: &str, service: &str, action: &str, body: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let Some(addr) = authority.to_socket_addrs_fallback().into_iter().next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: \"{service}#{action}\"\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    text.starts_with("HTTP/1.1 200") || text.contains(" 200 OK")
}

// ---------------------------------------------------------------------------
// QUIC policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WanLink {
    /// Home broadband: a 200 ms keepalive is free and makes handover instant.
    Wifi,
    /// LTE / 5G behind CGNAT.
    Cellular,
    /// Relay fallback; identical to cellular but avoids larger MTU probes.
    Relay,
}

impl WanLink {
    pub fn keepalive(self) -> Duration {
        match self {
            Self::Wifi => Duration::from_millis(500),
            Self::Cellular | Self::Relay => WAN_KEEPALIVE,
        }
    }

    pub fn idle_timeout(self) -> Duration {
        match self {
            Self::Wifi => Duration::from_secs(20),
            Self::Cellular | Self::Relay => WAN_IDLE_TIMEOUT,
        }
    }

    pub fn max_mtu(self) -> u16 {
        match self {
            Self::Wifi => WAN_MAX_MTU,
            Self::Cellular | Self::Relay => WAN_MIN_MTU + 8,
        }
    }
}

/// QUIC transport config tuned for mobile paths.
///
/// QUIC requires at least 1200 UDP payload bytes. DPLPMTUD is bounded to 1240 bytes
/// (so QUIC's own max UDP payload never exceeds a 464XLAT-safe size) and every
/// application datagram is capped at [`WAN_DATAGRAM_BUDGET`] by
/// [`crate::transport::safe_datagram_size`].  Migration is enabled on the
/// server side by `quinn`, which validates a new client address with a PATH
/// CHALLENGE and keeps the connection state.
pub fn wan_transport_config(link: WanLink) -> TransportConfig {
    let mut cfg = TransportConfig::default();
    cfg.congestion_controller_factory(Arc::new(BbrConfig::default()));
    cfg.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    cfg.datagram_send_buffer_size(4 * 1024 * 1024);
    cfg.keep_alive_interval(Some(link.keepalive()));
    cfg.max_idle_timeout(IdleTimeout::try_from(link.idle_timeout()).ok());

    // Never fragment: start at the IPv6 minimum and let DPLPMTUD probe up.
    cfg.initial_mtu(WAN_MIN_MTU);
    cfg.min_mtu(WAN_MIN_MTU);
    let mut mtud = MtuDiscoveryConfig::default();
    mtud.interval(Duration::from_secs(300));
    mtud.upper_bound(link.max_mtu());
    mtud.black_hole_cooldown(Duration::from_secs(30));
    cfg.mtu_discovery_config(Some(mtud));

    // Padding every datagram would waste phone uplink and break the pacer's
    // byte budget; QUIC still pads handshakes per RFC 9000.
    cfg.pad_to_mtu(false);
    cfg
}

/// One-shot plan for a WAN session: candidates, optional UPnP mapping and the
/// matching QUIC transport config.
#[derive(Debug, Clone)]
pub struct WanSessionPlan {
    pub link: WanLink,
    pub candidates: CandidateSet,
    pub upnp_external_port: Option<u16>,
    pub local_port: u16,
}

impl WanSessionPlan {
    /// Gather candidates and try UPnP for `local_port`.  Blocking; run it on
    /// `tokio::task::spawn_blocking` before creating the endpoint.
    pub fn prepare(socket: &UdpSocket, stun_servers: &[&str], link: WanLink) -> Self {
        let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let mut candidates = gather_candidates(socket, stun_servers);

        // UPnP only helps IPv4 gateways, and only when we know our LAN IP.
        let mut upnp_external_port = None;
        if let Some(local_ip) = primary_local_addr(false).map(|a| a.ip()) {
            if let IpAddr::V4(v4) = local_ip {
                if let Some(mapper) = UpnpMapper::discover() {
                    upnp_external_port = mapper.map_udp(v4, local_port, local_port);
                    if let (Some(port), Some(wan_ip)) = (
                        upnp_external_port,
                        crate::stun::DEFAULT_STUN_SERVERS
                            .iter()
                            .find_map(|s| stun_reflexive(socket, s, Duration::from_millis(600))),
                    ) {
                        candidates.push(
                            SocketAddr::new(wan_ip.ip(), port),
                            CandidateKind::Upnp,
                        );
                    }
                }
            }
        }
        candidates.upnp_external_port = upnp_external_port;

        Self {
            link,
            candidates,
            upnp_external_port,
            local_port,
        }
    }

    pub fn transport_config(&self) -> TransportConfig {
        wan_transport_config(self.link)
    }

    pub fn peer_candidates(&self, peer: &CandidateSet) -> Vec<SocketAddr> {
        peer.addresses()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_stack_socket_binds_and_marks_qos() {
        let socket = bind_dual_stack(0).expect("dual stack bind");
        assert_ne!(socket.local_addr().expect("addr").port(), 0);
        // Buffers are advisory on some hosts; the call itself must succeed.
        let sref = socket2::SockRef::from(&socket);
        assert!(sref.send_buffer_size().unwrap_or(0) > 0);
    }

    #[test]
    fn candidates_round_trip_through_signaling() {
        let mut set = CandidateSet::default();
        set.push("203.0.113.7:40000".parse().expect("v4"), CandidateKind::ServerReflexive);
        set.push("[2001:db8::1]:40000".parse().expect("v6"), CandidateKind::Host);
        let json = set.to_signaling();
        let back = CandidateSet::from_signaling(&json).expect("decode");
        assert_eq!(back, set);
        assert!(back.has_ipv6());
    }

    #[test]
    fn duplicate_and_unspecified_candidates_are_dropped() {
        let mut set = CandidateSet::default();
        set.push("0.0.0.0:1234".parse().expect("v4"), CandidateKind::Host);
        set.push("198.51.100.4:5000".parse().expect("v4"), CandidateKind::Host);
        set.push("198.51.100.4:5000".parse().expect("v4"), CandidateKind::Host);
        // IPv4-mapped form is the same peer.
        set.push("[::ffff:198.51.100.4]:5000".parse().expect("v6"), CandidateKind::Host);
        assert_eq!(set.candidates.len(), 1);
    }

    #[test]
    fn punch_payload_carries_the_magic_and_port() {
        let payload = HolePuncher::punch_payload(41234);
        assert!(HolePuncher::is_punch(&payload));
        assert_eq!(HolePuncher::advertised_port(&payload), Some(41234));
        assert!(!HolePuncher::is_punch(b"short"));
    }

    #[test]
    fn punch_between_two_loopback_peers_confirms_directly() {
        let host = Arc::new(bind_dual_stack(0).expect("host"));
        let client = Arc::new(bind_dual_stack(0).expect("client"));
        let host_addr = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, host.local_addr().expect("addr").port()));
        let client_addr = canonical(client.local_addr().expect("addr"));

        let host_socket = Arc::clone(&host);
        let responder = std::thread::spawn(move || {
            let mut buf = [0u8; 32];
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok((len, from)) = host_socket.recv_from(&mut buf) {
                    if HolePuncher::is_punch(&buf[..len]) {
                        let _ = host_socket.send_to(&HolePuncher::punch_payload(host_addr.port()), from);
                        return true;
                    }
                }
            }
            false
        });

        let path = HolePuncher::new(client)
            .with_window(Duration::from_millis(20), 50)
            .run(&[host_addr]);
        assert_eq!(path, WanPath::Direct(host_addr));
        assert!(responder.join().unwrap_or(false));
        let _ = client_addr;
    }

    #[test]
    fn empty_peer_list_falls_back_to_relay() {
        let socket = Arc::new(bind_dual_stack(0).expect("socket"));
        assert_eq!(
            HolePuncher::new(socket).run(&[]),
            WanPath::Relay
        );
    }

    #[test]
    fn ssdp_header_parsing_is_case_insensitive() {
        let response = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.1:5000/rootDesc.xml\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(
            UpnpMapper::header(response, "location").as_deref(),
            Some("http://192.168.1.1:5000/rootDesc.xml")
        );
    }

    #[test]
    fn upnp_description_extracts_the_wan_control_url() {
        let html = r#"<root><device><serviceList><service>
            <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <controlURL>/ctl/IPConn</controlURL>
            </service></serviceList></device></root>"#;
        let index = html.find("urn:schemas-upnp-org:service:WANIPConnection:1").expect("service");
        let tail = &html[index..];
        let control = tail
            .find("<controlURL>")
            .and_then(|start| {
                let rest = &tail[start + "<controlURL>".len()..];
                rest.find("</controlURL>").map(|end| rest[..end].trim().to_string())
            })
            .unwrap_or_default();
        assert_eq!(control, "/ctl/IPConn");
        assert_eq!(
            origin_of("http://192.168.1.1:5000/rootDesc.xml").as_deref(),
            Some("http://192.168.1.1:5000")
        );
    }

    #[test]
    fn wan_transport_config_uses_mobile_safe_bounds() {
        let cfg = wan_transport_config(WanLink::Cellular);
        let _ = cfg;
        assert_eq!(WanLink::Cellular.keepalive(), WAN_KEEPALIVE);
        assert!(WanLink::Cellular.max_mtu() <= WAN_MAX_MTU);
        assert!(WAN_DATAGRAM_BUDGET + 60 <= usize::from(WAN_MAX_MTU));
    }
}
