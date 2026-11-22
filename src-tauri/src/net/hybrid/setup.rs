use anyhow::{Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use ring::hmac;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    time::{interval, timeout},
};

use super::{raw::RawLink, rtc::RtcLink};
use crate::{
    net::{regional, regional_wire::TurnGrant},
    network::NetworkConfig,
    stun::{canonical_addr, gather_candidates, socket_target},
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const DIRECT_PROBE: Duration = Duration::from_millis(150);
const MAX_MESSAGE: usize = 32 * 1024;

pub(super) async fn write<T: Serialize>(send: &mut SendStream, message: &T) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "Handshake message is too large");
    timeout(SETUP_TIMEOUT, async {
        send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
        send.write_all(&bytes).await
    })
    .await??;
    Ok(())
}

pub(super) async fn read<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    timeout(SETUP_TIMEOUT, async {
        let mut header = [0; 4];
        recv.read_exact(&mut header).await?;
        let len = u32::from_be_bytes(header) as usize;
        ensure!(len <= MAX_MESSAGE, "Handshake message is too large");
        let mut bytes = vec![0; len];
        recv.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
}

#[derive(Serialize, Deserialize)]
struct Candidates {
    direct: Vec<SocketAddr>,
    key: [u8; 32],
    relay_only: bool,
}

pub(super) enum Exchange {
    Iroh { send: SendStream, recv: RecvStream },
    Regional(Box<regional::Exchange>),
}

impl Exchange {
    pub(super) async fn select(
        mut send: SendStream,
        mut recv: RecvStream,
        regional: Option<Result<regional::Exchange>>,
    ) -> Result<Self> {
        if let Some(regional) = regional {
            if let Err(error) = &regional {
                tracing::debug!(%error, "Regional signaling unavailable for this peer");
            }
            write(&mut send, &regional.is_ok()).await?;
            let ready: bool = read(&mut recv).await?;
            if let (true, Ok(regional)) = (ready, regional) {
                send.finish()?;
                tracing::debug!("Regional WebSocket selected for peer setup");
                return Ok(Self::Regional(Box::new(regional)));
            }
        }
        Ok(Self::Iroh { send, recv })
    }

    pub(super) async fn send<T: Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        match self {
            Self::Iroh { send, .. } => write(send, value).await,
            Self::Regional(exchange) => exchange.write(value).await,
        }
    }

    pub(super) async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        match self {
            Self::Iroh { recv, .. } => read(recv).await,
            Self::Regional(exchange) => exchange.read().await,
        }
    }

    pub(super) fn turn(&self) -> Option<&TurnGrant> {
        match self {
            Self::Regional(exchange) => Some(&exchange.turn),
            Self::Iroh { .. } => None,
        }
    }

    pub(super) async fn finish(self) -> Result<()> {
        match self {
            Self::Iroh { mut send, .. } => send.finish().map_err(Into::into),
            Self::Regional(exchange) => {
                exchange.finish().await;
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Direct {
    host: SocketAddr,
    client: SocketAddr,
}

pub(super) enum Transport {
    Raw(RawLink),
    Rtc(RtcLink),
}

pub(super) async fn negotiate(
    carrier: Connection,
    mut signaling: Exchange,
    config: &NetworkConfig,
    host: bool,
) -> Result<Transport> {
    let bind = || -> std::io::Result<UdpSocket> {
        let bound = if config.bind_addr.ip().is_unspecified() {
            crate::wan_transport::bind_dual_stack(0)
                .or_else(|_| std::net::UdpSocket::bind("0.0.0.0:0"))?
        } else {
            std::net::UdpSocket::bind(config.bind_addr)?
        };
        bound.set_nonblocking(true)?;
        crate::transport::tune_udp_socket(&bound)?;
        UdpSocket::from_std(bound)
    };
    let mut socket = bind()?;
    let stun = signaling
        .turn()
        .map(|grant| vec![format!("{}:{}", grant.domain, grant.udp_port)]);
    let stun = stun.as_deref().unwrap_or(&config.stun_servers);
    let local = if config.enable_stun && !config.relay_only {
        match timeout(DIRECT_PROBE, gather_candidates(&socket, stun, true)).await {
            Ok(candidates) => candidates,
            Err(_) => gather_candidates(&socket, &[], false).await,
        }
    } else {
        gather_candidates(&socket, &[], false).await
    };
    let mut key = [0; 32];
    getrandom::getrandom(&mut key).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let current = Candidates {
        direct: local.ordered_targets(),
        key,
        relay_only: config.relay_only,
    };
    signaling.send(&current).await?;
    let remote: Candidates = signaling.recv().await?;
    ensure!(remote.direct.len() <= 16, "Too many candidates");
    let relay_only = current.relay_only || remote.relay_only;
    let direct = if relay_only {
        None
    } else {
        probe(&socket, &current, &remote, host).await?
    };
    let decided: Option<Direct> = if host {
        signaling.send(&direct).await?;
        direct
    } else {
        signaling.recv().await?
    };
    if let Some(direct) = decided {
        let peer = if host { direct.client } else { direct.host };
        let local_addr = if host { direct.host } else { direct.client };
        // Both participants validate the addresses from the encrypted exchange.
        let valid = valid_candidate(peer) && current.direct.contains(&local_addr);
        signaling.send(&valid).await?;
        let remote_valid: bool = signaling.recv().await?;
        if !valid || !remote_valid {
            return Ok(Transport::Rtc(
                RtcLink::connect(socket, carrier, signaling, config, host, relay_only).await?,
            ));
        }
        let raw = RawLink::new(socket, peer, media_key(key), media_key(remote.key), host).await;
        signaling.send(&raw.is_ok()).await?;
        let remote_ready: bool = signaling.recv().await?;
        if let Ok(raw) = raw
            && remote_ready
        {
            signaling.finish().await?;
            return Ok(Transport::Raw(raw));
        }
        socket = bind()?;
    }
    let rtc = RtcLink::connect(socket, carrier, signaling, config, host, relay_only).await?;
    Ok(Transport::Rtc(rtc))
}

fn media_key(key: [u8; 32]) -> [u8; 32] {
    hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, &key),
        b"vmax raw media encryption/1",
    )
    .as_ref()
    .try_into()
    .expect("SHA-256 tag is 32 bytes")
}

fn valid_candidate(addr: SocketAddr) -> bool {
    addr.port() != 0
        && !addr.ip().is_unspecified()
        && !addr.ip().is_multicast()
        && !matches!(addr.ip(), IpAddr::V4(ip) if ip.is_broadcast())
}

async fn probe(
    socket: &UdpSocket,
    local: &Candidates,
    remote: &Candidates,
    host: bool,
) -> Result<Option<Direct>> {
    let outgoing = hmac::Key::new(hmac::HMAC_SHA256, &local.key);
    let incoming = hmac::Key::new(hmac::HMAC_SHA256, &remote.key);
    let ipv6 = socket.local_addr()?.is_ipv6();
    const PROBE_DATA: usize = 30;
    let mut buf = [0; PROBE_DATA + 32];
    let mut ticks = interval(Duration::from_millis(25));
    let deadline = tokio::time::sleep(DIRECT_PROBE);
    tokio::pin!(deadline);
    let mut selected = None;
    let mut nonce = [0; 8];
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    loop {
        tokio::select! {
            _ = &mut deadline => return Ok(selected),
            _ = ticks.tick() => for (index, &target) in remote.direct.iter().enumerate() {
                if !valid_candidate(target) { continue; }
                let msg = probe_packet(&outgoing, 1, index as u8, nonce, None);
                let _ = socket.send_to(&msg, socket_target(target, ipv6)).await;
            },
            received = socket.recv_from(&mut buf) => {
                let Ok((size, from)) = received else { continue };
                if size != buf.len() || hmac::verify(&incoming, &buf[..PROBE_DATA], &buf[PROBE_DATA..]).is_err() { continue; }
                let from = canonical_addr(from);
                if buf[0] == 1 {
                    let msg = probe_packet(&outgoing, 2, buf[1], buf[2..10].try_into()?, Some(from));
                    let _ = socket.send_to(&msg, socket_target(from, ipv6)).await;
                } else if host && selected.is_none() && buf[0] == 2 && buf[2..10] == nonce {
                    if remote.direct.get(usize::from(buf[1])).is_none() { continue; }
                    let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&buf[10..26])?);
                    let host_addr = canonical_addr(SocketAddr::new(ip.into(), u16::from_be_bytes(buf[26..28].try_into()?)));
                    if local.direct.contains(&host_addr) {
                        selected = Some(Direct { host: host_addr, client: from });
                    }
                }
            }
        }
    }
}

fn probe_packet(
    key: &hmac::Key,
    kind: u8,
    index: u8,
    nonce: [u8; 8],
    observed: Option<SocketAddr>,
) -> [u8; 62] {
    let mut bytes = [0; 62];
    bytes[..2].copy_from_slice(&[kind, index]);
    bytes[2..10].copy_from_slice(&nonce);
    if let Some(addr) = observed {
        let ip = match addr.ip() {
            IpAddr::V4(ip) => ip.to_ipv6_mapped(),
            IpAddr::V6(ip) => ip,
        };
        bytes[10..26].copy_from_slice(&ip.octets());
        bytes[26..28].copy_from_slice(&addr.port().to_be_bytes());
    }
    let tag = hmac::sign(key, &bytes[..30]);
    bytes[30..].copy_from_slice(tag.as_ref());
    bytes
}
