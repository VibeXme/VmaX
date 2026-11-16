use crate::stun::{canonical_addr, socket_target};
use ring::hmac;
use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    net::UdpSocket,
    sync::watch,
    time::{Instant, interval},
};

const PROBE_LEN: usize = 8 + 8 + 32;
const PROBE_MAGIC: &[u8; 6] = b"VMXPTH";
const PROBE_INTERVAL: Duration = Duration::from_millis(400);
const PATH_LEASE: Duration = Duration::from_millis(1600);

fn probe(key: &hmac::Key, side: u8, response: bool, nonce: [u8; 8]) -> [u8; PROBE_LEN] {
    let mut out = [0; PROBE_LEN];
    out[..6].copy_from_slice(PROBE_MAGIC);
    out[6] = side;
    out[7] = u8::from(response);
    out[8..16].copy_from_slice(&nonce);
    let tag = hmac::sign(key, &out[..16]);
    out[16..].copy_from_slice(tag.as_ref());
    out
}

pub(crate) struct RelayUpgrade {
    pub udp: std::sync::Arc<UdpSocket>,
    pub candidates: Vec<SocketAddr>,
    pub host: bool,
    pub key: watch::Receiver<Option<[u8; 32]>>,
    pub path: watch::Sender<Option<SocketAddr>>,
}

impl RelayUpgrade {
    /// Feed both routes into the same virtual QUIC address. Only an
    /// authenticated probe response can move outgoing traffic off relay.
    pub async fn receive(mut self, bridge: std::sync::Arc<UdpSocket>) -> io::Result<()> {
        let key = self
            .key
            .wait_for(Option::is_some)
            .await
            .map_err(io::Error::other)?
            .expect("key supplied");
        let key = hmac::Key::new(hmac::HMAC_SHA256, &key);
        let side = u8::from(self.host);
        let ipv6 = self.udp.local_addr()?.is_ipv6();
        let mut timer = interval(PROBE_INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut recent = [(None, [0; 8]); 4];
        let mut next_nonce = 0;
        let mut validated = None::<(SocketAddr, Instant)>;
        let mut peers = Vec::<(SocketAddr, Instant)>::with_capacity(8);
        let mut buf = [0; 4096];
        loop {
            tokio::select! {
                _ = timer.tick() => {
                    let now = Instant::now();
                    if validated.is_some_and(|(_, seen)| now.duration_since(seen) >= PATH_LEASE) {
                        validated = None;
                        self.path.send_replace(None);
                    }
                    peers.retain(|(_, seen)| now.duration_since(*seen) < PATH_LEASE);
                    let nonce = rand::random::<[u8; 8]>();
                    recent[next_nonce] = (Some(now), nonce);
                    next_nonce = (next_nonce + 1) % recent.len();
                    let packet = probe(&key, side, false, nonce);
                    if let Some((address, _)) = validated {
                        let _ = self.udp.send_to(&packet, address).await;
                    } else {
                        for address in self.candidates.iter().copied().take(16).map(|addr| socket_target(addr, ipv6))
                            .chain(peers.iter().map(|(addr, _)| *addr))
                        {
                            let _ = self.udp.send_to(&packet, address).await;
                        }
                    }
                }
                packet = self.udp.recv_from(&mut buf) => {
                    let (len, from) = match packet {
                        Ok(packet) => packet,
                        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => continue,
                        Err(error) => return Err(error),
                    };
                    let packet = &buf[..len];
                    if packet.starts_with(PROBE_MAGIC) {
                        if len != PROBE_LEN || packet[6] != side ^ 1
                            || hmac::verify(&key, &packet[..16], &packet[16..]).is_err()
                        {
                            continue;
                        }
                        let nonce: [u8; 8] = packet[8..16].try_into().expect("fixed nonce length");
                        let now = Instant::now();
                        match packet[7] {
                            0 => {
                                if let Some((_, seen)) = peers.iter_mut().find(|(addr, _)| *addr == from) {
                                    *seen = now;
                                } else if peers.len() < 8 {
                                    peers.push((from, now));
                                }
                                let _ = self.udp.send_to(&probe(&key, side, true, nonce), from).await;
                            }
                            1 if recent.iter().any(|(sent, n)| *n == nonce
                                && sent.is_some_and(|ts| now.duration_since(ts) < PATH_LEASE)) =>
                            {
                                if validated.is_none_or(|(address, _)| canonical_addr(address) == canonical_addr(from)) {
                                    // Consume a response once so duplicates cannot renew the lease.
                                    if let Some((sent, _)) = recent.iter_mut().find(|(sent, n)| sent.is_some() && *n == nonce) {
                                        *sent = None;
                                    }
                                    if validated.is_none() { self.path.send_replace(Some(from)); }
                                    validated = Some((from, now));
                                }
                            }
                            _ => {}
                        }
                    } else if validated.is_some_and(|(addr, _)| addr == from)
                        || peers.iter().any(|(addr, _)| *addr == from)
                    {
                        // QUIC authenticates packets arriving through the bridge.
                        bridge.send(packet).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::time::timeout;

    #[tokio::test]
    async fn authenticated_paths_forward_media_then_expire_without_peer() {
        let left = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let right = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let bridge = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        bridge.connect(app.local_addr().unwrap()).await.unwrap();
        let (key, key_rx) = watch::channel(Some([23; 32]));
        let (left_path, mut left_rx) = watch::channel(None);
        let (right_path, mut right_rx) = watch::channel(None);
        let left_addr = left.local_addr().unwrap();
        let right_addr = right.local_addr().unwrap();
        let host = tokio::spawn(
            RelayUpgrade {
                udp: left.clone(),
                candidates: vec![right_addr],
                host: true,
                key: key_rx.clone(),
                path: left_path,
            }
            .receive(bridge.clone()),
        );
        let client = tokio::spawn(
            RelayUpgrade {
                udp: right.clone(),
                candidates: vec![left_addr],
                host: false,
                key: key_rx,
                path: right_path,
            }
            .receive(bridge),
        );
        timeout(Duration::from_secs(3), async {
            assert_eq!(
                *left_rx.wait_for(Option::is_some).await.unwrap(),
                Some(right_addr)
            );
            assert_eq!(
                *right_rx.wait_for(Option::is_some).await.unwrap(),
                Some(left_addr)
            );
            left.send_to(b"QUIC payload", right_addr).await.unwrap();
            let mut received = [0; 32];
            let len = app.recv(&mut received).await.unwrap();
            assert_eq!(&received[..len], b"QUIC payload");
            client.abort();
            assert!(left_rx.wait_for(Option::is_none).await.is_ok());
        })
        .await
        .unwrap();
        host.abort();
        drop(key);
    }
}
