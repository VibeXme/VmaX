use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, time::timeout};
use turn::client::{Client, ClientConfig};
use webrtc_util::Conn;

pub(crate) struct TurnRelay {
    client: Client,
    allocation: Option<Arc<dyn Conn + Send + Sync>>,
}

impl TurnRelay {
    pub(crate) async fn configured() -> Option<Self> {
        let server = std::env::var("VMAX_TURN_SERVER").ok()?;
        let username = std::env::var("VMAX_TURN_USERNAME").ok()?;
        let password = std::env::var("VMAX_TURN_PASSWORD").ok()?;
        match timeout(
            Duration::from_millis(1500),
            Self::allocate(server, username, password),
        )
        .await
        {
            Ok(Ok(relay)) => Some(relay),
            _ => {
                tracing::debug!("TURN allocation unavailable; using configured fallback");
                None
            }
        }
    }

    async fn allocate(server: String, username: String, password: String) -> io::Result<Self> {
        let conn = UdpSocket::bind("0.0.0.0:0").await?;
        let client = Client::new(ClientConfig {
            stun_serv_addr: server.clone(),
            turn_serv_addr: server,
            username,
            password,
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: Arc::new(conn),
            vnet: None,
        })
        .await
        .map_err(io::Error::other)?;
        let mut relay = Self {
            client,
            allocation: None,
        };
        relay.client.listen().await.map_err(io::Error::other)?;
        let allocation = relay.client.allocate().await.map_err(io::Error::other)?;
        allocation.local_addr().map_err(io::Error::other)?;
        relay.allocation = Some(Arc::new(allocation));
        Ok(relay)
    }

    pub(crate) fn conn(&self) -> Arc<dyn Conn + Send + Sync> {
        Arc::clone(self.allocation.as_ref().expect("successful allocation"))
    }

    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        self.allocation.as_ref()?.local_addr().ok()
    }
}

impl Drop for TurnRelay {
    fn drop(&mut self) {
        let client = self.client.clone();
        let allocation = self.allocation.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(allocation) = allocation {
                    let _ = timeout(Duration::from_secs(1), allocation.close()).await;
                }
                let _ = client.close().await;
            });
        }
    }
}

// Do not ask an internet relay to send probes to a peer's advertised LANs.
pub(crate) fn is_public_peer(addr: SocketAddr) -> bool {
    let std::net::IpAddr::V4(ip) = crate::stun::canonical_addr(addr).ip() else {
        return false;
    };
    let [first, second, _, _] = ip.octets();
    addr.port() != 0
        && first != 0
        && first < 224
        && !(first == 100 && (64..=127).contains(&second))
        && !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_documentation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_upgrade::{RelayUpgrade, UdpRoutes};
    use crate::signaling::DatagramRoute;
    use tokio::sync::watch;
    use turn::{
        auth::StaticAuthHandler,
        relay::relay_static::RelayAddressGeneratorStatic,
        server::{
            Server,
            config::{ConnConfig, ServerConfig},
        },
    };
    use webrtc_util::vnet::net::Net;

    #[tokio::test]
    async fn authenticates_and_moves_packets_over_a_real_turn_allocation() {
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_socket.local_addr().unwrap();
        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn: Arc::new(server_socket),
                relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                    relay_address: "127.0.0.1".parse().unwrap(),
                    address: "127.0.0.1".into(),
                    net: Arc::new(Net::new(None)),
                }),
            }],
            realm: "vmax-test".into(),
            auth_handler: Arc::new(
                StaticAuthHandler::new([("viewer".into(), "test-password".into())].into()).unwrap(),
            ),
            channel_bind_timeout: Duration::ZERO,
            alloc_close_notify: None,
        })
        .await
        .unwrap();

        timeout(Duration::from_secs(5), async {
            let relay = TurnRelay::allocate(
                server_addr.to_string(),
                "viewer".into(),
                "test-password".into(),
            )
            .await
            .unwrap();
            let turn_addr = relay.local_addr().unwrap();
            let peer = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let peer_addr = peer.local_addr().unwrap();
            let host_app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let peer_app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let host_bridge = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let peer_bridge = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            host_bridge
                .connect(host_app.local_addr().unwrap())
                .await
                .unwrap();
            peer_bridge
                .connect(peer_app.local_addr().unwrap())
                .await
                .unwrap();
            let (_key_tx, key) = watch::channel(Some([31; 32]));
            let (host_path, mut host_rx) = watch::channel(None);
            let (peer_path, mut peer_rx) = watch::channel(None);
            let host_receive = tokio::spawn(
                RelayUpgrade {
                    udp: relay.conn(),
                    candidates: vec![peer_addr],
                    fallback_candidate: None,
                    host: true,
                    key: key.clone(),
                    path: host_path,
                }
                .receive(host_bridge),
            );
            let peer_receive = tokio::spawn(
                RelayUpgrade {
                    udp: peer.clone(),
                    candidates: vec![],
                    fallback_candidate: Some(turn_addr),
                    host: false,
                    key,
                    path: peer_path,
                }
                .receive(peer_bridge),
            );

            assert_eq!(
                *host_rx.wait_for(Option::is_some).await.unwrap(),
                Some(peer_addr)
            );
            assert_eq!(
                *peer_rx.wait_for(Option::is_some).await.unwrap(),
                Some(turn_addr)
            );
            let routes = UdpRoutes {
                direct: (None, watch::channel(None).1),
                turn: (Some(relay.conn()), host_rx),
            };
            assert!(
                tokio::spawn(async move { routes.try_send(b"host media").await })
                    .await
                    .unwrap()
            );
            peer.send_to(b"peer input", turn_addr).await.unwrap();
            let mut packet = [0; 32];
            let size = peer_app.recv(&mut packet).await.unwrap();
            assert_eq!(&packet[..size], b"host media");
            let size = host_app.recv(&mut packet).await.unwrap();
            assert_eq!(&packet[..size], b"peer input");
            host_receive.abort();
            peer_receive.abort();
        })
        .await
        .unwrap();
        server.close().await.unwrap();
    }
}
