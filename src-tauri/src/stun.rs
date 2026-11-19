use crate::vmax_relay_ice::{IceEndpoint, default_free_ice_endpoints};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Duration;
use thiserror::Error;
use tokio::net::{UdpSocket, lookup_host};
use tokio::time::{Instant, sleep, sleep_until};

const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_RESPONSE: u16 = 0x0101;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Cloudflare Anycast STUN first for lowest latency in India, followed by Google & Twilio STUN
pub const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "global.stun.twilio.com:3478",
];

/// Exposes free STUN + TURN endpoints (Cloudflare Realtime TURN 1TB/mo free & Metered Open Relay)
pub fn free_ice_endpoints() -> Vec<IceEndpoint> {
    default_free_ice_endpoints()
}

#[derive(Debug, Error)]
pub enum StunError {
    #[error("I/O error during STUN transaction: {0}")]
    Io(#[from] io::Error),
    #[error("invalid STUN response packet")]
    MalformedResponse,
}

#[derive(Debug, Clone)]
pub struct IceCandidates {
    pub local_addr: SocketAddr,
    pub lan_addrs: Vec<SocketAddr>,
    pub public_reflexive: Option<SocketAddr>,
    pub public_reflexive_v6: Option<SocketAddr>,
    /// External IPv4 ports the STUN probes were told about, in answer order.
    /// Two different ports mean the NAT allocates one mapping per destination
    /// (symmetric), which is what makes port prediction necessary.
    pub reflexive_ports: Vec<u16>,
}

pub(crate) fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) if let Some(ip) = v6.ip().to_ipv4_mapped() => {
            SocketAddr::new(ip.into(), v6.port())
        }
        _ => addr,
    }
}

pub(crate) fn socket_target(addr: SocketAddr, ipv6_socket: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if ipv6_socket => {
            SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())
        }
        _ => addr,
    }
}

fn is_tailscale_cgnat(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (64..=127).contains(&b)
        }
        IpAddr::V6(_) => false,
    }
}

impl IceCandidates {
    /// Returns ordered candidate addresses:
    /// 1) Private LAN IPs, 2) Tailscale mesh IPs (100.64.0.0/10), 3) Public STUN reflexive IP.
    pub fn ordered_targets(&self) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        for addr in &self.lan_addrs {
            if !is_tailscale_cgnat(addr.ip()) && !out.contains(addr) {
                out.push(*addr);
            }
        }
        for addr in &self.lan_addrs {
            if is_tailscale_cgnat(addr.ip()) && !out.contains(addr) {
                out.push(*addr);
            }
        }
        if let Some(pub_addr) = self.public_reflexive
            && !out.contains(&pub_addr)
        {
            out.push(pub_addr);
        }
        if let Some(pub_addr) = self.public_reflexive_v6
            && !out.contains(&pub_addr)
        {
            out.push(pub_addr);
        }
        if !out.contains(&self.local_addr) && !self.local_addr.ip().is_unspecified() {
            out.push(self.local_addr);
        }
        out
    }
}

/// Builds a 20-byte RFC 5389 STUN Binding Request.
pub fn build_binding_request(tx_id: [u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    buf[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    buf[8..20].copy_from_slice(&tx_id);
    buf
}

/// Parses an RFC 5389 STUN Binding Success Response and extracts the mapped public `SocketAddr`.
pub fn parse_binding_response(
    packet: &[u8],
    expected_tx_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if packet.len() < 20 {
        return Err(StunError::MalformedResponse);
    }
    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
    let msg_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let cookie = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);

    if msg_type != STUN_BINDING_RESPONSE
        || cookie != STUN_MAGIC_COOKIE
        || &packet[8..20] != expected_tx_id
        || packet.len() < 20 + msg_len
    {
        return Err(StunError::MalformedResponse);
    }

    let mut offset = 20;
    let end = 20 + msg_len;
    let mut fallback_mapped: Option<SocketAddr> = None;

    while offset + 4 <= end {
        let attr_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let attr_len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let val_start = offset + 4;
        let val_end = val_start + attr_len;
        if val_end > end {
            return Err(StunError::MalformedResponse);
        }
        let val = &packet[val_start..val_end];

        if attr_type == ATTR_XOR_MAPPED_ADDRESS
            && let Some(addr) = decode_address_attr(val, true, expected_tx_id)
        {
            return Ok(addr);
        } else if attr_type == ATTR_MAPPED_ADDRESS && fallback_mapped.is_none() {
            fallback_mapped = decode_address_attr(val, false, expected_tx_id);
        }

        let padded_len = (attr_len + 3) & !3;
        offset = val_start + padded_len;
    }

    fallback_mapped.ok_or(StunError::MalformedResponse)
}

fn decode_address_attr(val: &[u8], xor: bool, tx_id: &[u8; 12]) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let raw_port = u16::from_be_bytes([val[2], val[3]]);
    let port = if xor {
        raw_port ^ ((STUN_MAGIC_COOKIE >> 16) as u16)
    } else {
        raw_port
    };

    match family {
        0x01 if val.len() >= 8 => {
            let mut ip_bytes: [u8; 4] = val[4..8].try_into().ok()?;
            if xor {
                let cookie_bytes = STUN_MAGIC_COOKIE.to_be_bytes();
                for i in 0..4 {
                    ip_bytes[i] ^= cookie_bytes[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port))
        }
        0x02 if val.len() >= 20 => {
            let mut ip_bytes: [u8; 16] = val[4..20].try_into().ok()?;
            if xor {
                let cookie_bytes = STUN_MAGIC_COOKIE.to_be_bytes();
                for i in 0..4 {
                    ip_bytes[i] ^= cookie_bytes[i];
                }
                for i in 0..12 {
                    ip_bytes[4 + i] ^= tx_id[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port))
        }
        _ => None,
    }
}

/// Result of the reflexive probe: one mapped address per family plus every
/// external port that was observed, which is what NAT classification reads.
#[derive(Debug, Default)]
struct ReflexiveProbe {
    mapped: [Option<SocketAddr>; 2],
    ports: Vec<u16>,
}

/// Races STUN endpoints per address family with a single receive loop.
async fn discover_reflexive_addrs(
    socket: &UdpSocket,
    stun_servers: &[&str],
    attempt: Duration,
    ipv4: bool,
    ipv6: bool,
) -> ReflexiveProbe {
    let mut mapped_addrs = [None, None];
    let mut ports: Vec<u16> = Vec::new();
    let family_enabled = [ipv4, ipv6];
    let mut resolving: FuturesUnordered<_> = stun_servers
        .iter()
        .map(|&server| async move { lookup_host(server).await.ok() })
        .collect();
    let mut requests = HashMap::new();
    let mut deadline = Instant::now() + attempt * 2;
    let retry = sleep(attempt);
    tokio::pin!(retry);
    let mut retried = false;
    let mut recv_buf = [0u8; 512];
    loop {
        if resolving.is_empty() && requests.is_empty() {
            break;
        }
        tokio::select! {
            _ = sleep_until(deadline) => break,
            Some(resolved) = resolving.next() => {
                if let Some(addresses) = resolved {
                    let mut tried = [false; 2];
                    for target in addresses {
                        let family = usize::from(target.is_ipv6());
                        if !family_enabled[family] || tried[family] || mapped_addrs[family].is_some() {
                            continue;
                        }
                        tried[family] = true;
                        if let std::collections::hash_map::Entry::Vacant(entry) = requests.entry(target) {
                            let tx_id = rand::random();
                            if socket.send_to(&build_binding_request(tx_id), socket_target(target, ipv6)).await.is_ok() {
                                entry.insert(tx_id);
                            }
                        }
                    }
                }
            }
            _ = &mut retry, if !retried => {
                retried = true;
                for (&target, &tx_id) in &requests {
                    let _ = socket.send_to(&build_binding_request(tx_id), socket_target(target, ipv6)).await;
                }
            }
            result = socket.recv_from(&mut recv_buf), if !requests.is_empty() => {
                match result {
                    Ok((len, from)) => {
                        let from = canonical_addr(from);
                        let family = usize::from(from.is_ipv6());
                        if let Some(tx_id) = requests.get(&from)
                            && let Ok(mapped) = parse_binding_response(&recv_buf[..len], tx_id)
                            && canonical_addr(mapped).is_ipv6() == from.is_ipv6()
                            && mapped.port() != 0
                        {
                            let mapped = canonical_addr(mapped);
                            mapped_addrs[family] = mapped_addrs[family].or(Some(mapped));
                            if !ports.contains(&mapped.port()) {
                                ports.push(mapped.port());
                            }
                            // One answer per family is already enough to start;
                            // a second one is what reveals a symmetric NAT, so
                            // it is given at most another 75 ms to arrive.
                            if (0..2).all(|i| !family_enabled[i] || mapped_addrs[i].is_some())
                                && ports.len() >= 2
                            {
                                break;
                            }
                            if ports.len() >= 2 {
                                requests.retain(|target, _| target.is_ipv6() != from.is_ipv6());
                            }
                            deadline = deadline.min(Instant::now() + Duration::from_millis(75));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                    Err(_) => break,
                }
            }
        }
    }
    ReflexiveProbe {
        mapped: mapped_addrs,
        ports,
    }
}

/// Gathers local LAN + STUN public reflexive candidates using the given UDP socket.
pub async fn gather_candidates(
    socket: &UdpSocket,
    stun_servers: &[String],
    enable_stun: bool,
) -> IceCandidates {
    let local_addr = socket
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0));
    let port = local_addr.port();
    let ipv6 = local_addr.is_ipv6();
    let ipv4 = local_addr.is_ipv4()
        || socket2::SockRef::from(socket)
            .only_v6()
            .is_ok_and(|only_v6| !only_v6);

    let mut lan_addrs = Vec::new();
    if !local_addr.ip().is_unspecified() {
        lan_addrs.push(canonical_addr(local_addr));
    } else {
        if ipv4 {
            if let Some(primary_ip) = discover_primary_lan_ip() {
                lan_addrs.push(SocketAddr::new(primary_ip, port));
            }
            lan_addrs.push(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port));
        }
        if ipv6 && let Some(ip) = discover_routed_ip("[2606:4700:4700::1111]:53") {
            lan_addrs.push(SocketAddr::new(ip, port));
        }
    }

    let probe = if enable_stun {
        let servers: Vec<&str> = if stun_servers.is_empty() {
            DEFAULT_STUN_SERVERS.to_vec()
        } else {
            stun_servers.iter().map(String::as_str).collect()
        };
        discover_reflexive_addrs(socket, &servers, Duration::from_millis(650), ipv4, ipv6).await
    } else {
        ReflexiveProbe::default()
    };

    IceCandidates {
        local_addr,
        lan_addrs,
        public_reflexive: probe.mapped[0],
        public_reflexive_v6: probe.mapped[1],
        reflexive_ports: probe.ports,
    }
}

pub fn discover_primary_lan_ip() -> Option<IpAddr> {
    discover_routed_ip("8.8.8.8:80")
}

fn discover_routed_ip(target: &str) -> Option<IpAddr> {
    let target: SocketAddr = target.parse().ok()?;
    let sock = StdUdpSocket::bind(if target.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .ok()?;
    sock.connect(target).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_unspecified() { None } else { Some(ip) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_response(tx_id: [u8; 12], ip: [u8; 4], port: u16) -> Vec<u8> {
        let xport = port ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        let xip = [
            ip[0] ^ cookie[0],
            ip[1] ^ cookie[1],
            ip[2] ^ cookie[2],
            ip[3] ^ cookie[3],
        ];

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&STUN_BINDING_RESPONSE.to_be_bytes());
        pkt.extend_from_slice(&12u16.to_be_bytes());
        pkt.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(&tx_id);
        pkt.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        pkt.extend_from_slice(&8u16.to_be_bytes());
        pkt.push(0x00);
        pkt.push(0x01);
        pkt.extend_from_slice(&xport.to_be_bytes());
        pkt.extend_from_slice(&xip);
        pkt
    }

    #[test]
    fn parses_rfc5389_xor_mapped_ipv4() {
        let tx_id = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
        ];
        let port = 51234;
        let pkt = binding_response(tx_id, [203, 0, 113, 45], port);

        let parsed = parse_binding_response(&pkt, &tx_id).unwrap();
        assert_eq!(parsed, "203.0.113.45:51234".parse::<SocketAddr>().unwrap());
    }

    #[tokio::test]
    async fn responsive_stun_server_wins_without_waiting_for_silent_server() {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addresses = [
            silent.local_addr().unwrap().to_string(),
            server.local_addr().unwrap().to_string(),
        ];
        let reply = async {
            let mut request = [0; 20];
            let (len, from) = server.recv_from(&mut request).await.unwrap();
            assert_eq!(len, request.len());
            assert_eq!(from, client.local_addr().unwrap());
            let tx_id: [u8; 12] = request[8..].try_into().unwrap();
            let mut wrong_id = tx_id;
            wrong_id[0] ^= 1;
            server
                .send_to(&binding_response(wrong_id, [203, 0, 113, 1], 51234), from)
                .await
                .unwrap();
            server
                .send_to(&binding_response(tx_id, [203, 0, 113, 45], 51234), from)
                .await
                .unwrap();
        };
        let attempt = Duration::from_secs(3);
        let servers = addresses.each_ref().map(String::as_str);
        let (mapped, ()) = tokio::time::timeout(attempt, async {
            tokio::join!(
                discover_reflexive_addrs(&client, &servers, attempt, true, false),
                reply
            )
        })
        .await
        .expect("must finish before retrying the silent endpoint");
        assert_eq!(
            mapped.mapped,
            [Some("203.0.113.45:51234".parse().unwrap()), None]
        );
    }

    #[tokio::test]
    async fn dual_stack_stun_advertises_both_families_from_one_media_socket() {
        let client = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        client.set_only_v6(false).unwrap();
        client
            .bind(&"[::]:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        client.set_nonblocking(true).unwrap();
        let client = UdpSocket::from_std(client.into()).unwrap();
        let server_v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_v6 = UdpSocket::bind("[::1]:0").await.unwrap();
        let servers = [
            server_v4.local_addr().unwrap().to_string(),
            server_v6.local_addr().unwrap().to_string(),
        ];
        async fn echo_mapped_address(server: &UdpSocket, port: u16) {
            let mut request = [0; 20];
            let (len, from) = server.recv_from(&mut request).await.unwrap();
            assert_eq!(len, request.len());
            assert_eq!(from.port(), port);
            let mut response = Vec::new();
            let value_len = if from.is_ipv4() { 8u16 } else { 20u16 };
            response.extend(STUN_BINDING_RESPONSE.to_be_bytes());
            response.extend((4 + value_len).to_be_bytes());
            response.extend_from_slice(&request[4..]);
            response.extend(ATTR_MAPPED_ADDRESS.to_be_bytes());
            response.extend(value_len.to_be_bytes());
            response.extend([0, if from.is_ipv4() { 1 } else { 2 }]);
            response.extend(port.to_be_bytes());
            match from.ip() {
                IpAddr::V4(ip) => response.extend(ip.octets()),
                IpAddr::V6(ip) => response.extend(ip.octets()),
            }
            server.send_to(&response, from).await.unwrap();
        }
        let port = client.local_addr().unwrap().port();
        let (candidates, (), ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                gather_candidates(&client, &servers, true),
                echo_mapped_address(&server_v4, port),
                echo_mapped_address(&server_v6, port),
            )
        })
        .await
        .unwrap();
        assert_eq!(
            candidates.public_reflexive,
            Some((Ipv4Addr::LOCALHOST, port).into())
        );
        assert_eq!(
            candidates.public_reflexive_v6,
            Some((Ipv6Addr::LOCALHOST, port).into())
        );
    }
}
