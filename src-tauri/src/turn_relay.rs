use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::json;
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, time::timeout};
use turn::client::{Client, ClientConfig};
use url::Url;
use webrtc_util::Conn;
#[path = "turn_tls.rs"]
mod turn_tls;

const ALLOCATION_TIMEOUT: Duration = Duration::from_millis(1500);
const CLOUDFLARE_TIMEOUT: Duration = Duration::from_secs(2);
const CLOUDFLARE_TTL_SECONDS: u32 = 86_400;
const MAX_CREDENTIAL_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_TURN_ENDPOINTS: usize = 4;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IceServers {
    ice_servers: Vec<IceServer>,
}

#[derive(Deserialize)]
struct IceServer {
    urls: IceUrls,
    username: Option<String>,
    credential: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IceUrls {
    Single(String),
    Multiple(Vec<String>),
}

struct TurnCredentials {
    server: String,
    username: String,
    password: String,
}

fn udp_turn_endpoint(url: &str) -> Option<String> {
    let address = url.strip_prefix("turn:")?;
    let (host, transport) = address
        .split_once('?')
        .unwrap_or((address, "transport=udp"));
    if transport != "transport=udp" || host.contains(['/', '@', '#', '\\']) {
        return None;
    }
    let address = Url::parse(&format!("turn://{host}")).ok()?;
    let host = match address.host()? {
        url::Host::Domain(domain) => domain.to_owned(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(_) => return None,
    };
    let port = address.port().unwrap_or(3478);
    (port != 0).then(|| format!("{host}:{port}"))
}

impl IceServers {
    fn udp_credentials(self) -> Vec<TurnCredentials> {
        let mut endpoints = Vec::<TurnCredentials>::new();
        for entry in self.ice_servers {
            let (Some(username), Some(password)) = (entry.username, entry.credential) else {
                continue;
            };
            if username.is_empty() || password.is_empty() {
                continue;
            }
            let urls = match entry.urls {
                IceUrls::Single(url) => vec![url],
                IceUrls::Multiple(urls) => urls,
            };
            for url in urls {
                if let Some(server) = udp_turn_endpoint(&url)
                    && !endpoints.iter().any(|entry| entry.server == server)
                {
                    endpoints.push(TurnCredentials {
                        server,
                        username: username.clone(),
                        password: password.clone(),
                    });
                    if endpoints.len() == MAX_TURN_ENDPOINTS {
                        return endpoints;
                    }
                }
            }
        }
        endpoints
    }
}

fn cloudflare_endpoint(key_id: &str) -> Option<Url> {
    if key_id.is_empty()
        || key_id.len() > 128
        || !key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Url::parse(&format!(
        "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
    ))
    .ok()
}

async fn fetch_cloudflare_credentials(endpoint: Url, token: &str) -> Option<Vec<TurnCredentials>> {
    let client = reqwest::Client::builder()
        .timeout(CLOUDFLARE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let mut response = client
        .post(endpoint)
        .bearer_auth(token)
        .json(&json!({ "ttl": CLOUDFLARE_TTL_SECONDS }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_CREDENTIAL_RESPONSE_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(
        serde_json::from_slice::<IceServers>(&body)
            .ok()?
            .udp_credentials(),
    )
}

pub(crate) struct TurnRelay {
    client: Client,
    allocation: Option<Arc<dyn Conn + Send + Sync>>,
}

impl TurnRelay {
    pub(crate) async fn regional(
        domain: &str,
        udp_port: u16,
        tls_port: u16,
        username: String,
        password: String,
    ) -> Option<Self> {
        if let Some(relay) = timeout(
            ALLOCATION_TIMEOUT,
            Self::allocate(
                format!("{domain}:{udp_port}"),
                username.clone(),
                password.clone(),
            ),
        )
        .await
        .ok()
        .and_then(Result::ok)
        {
            return Some(relay);
        }

        timeout(Duration::from_secs(3), async {
            let conn = turn_tls::TlsConn::connect(domain, tls_port).await?;
            let addr = conn.remote_addr().expect("TLS TURN connection has a peer");
            Self::allocate_on(addr.to_string(), username, password, Arc::new(conn)).await
        })
        .await
        .ok()
        .and_then(Result::ok)
    }

    pub(crate) async fn configured() -> Option<Self> {
        if let (Ok(key_id), Ok(token)) = (
            std::env::var("VMAX_CLOUDFLARE_TURN_KEY_ID"),
            std::env::var("VMAX_CLOUDFLARE_TURN_API_TOKEN"),
        ) {
            if let Some(endpoint) = cloudflare_endpoint(&key_id)
                && !token.is_empty()
                && let Some(credentials) = fetch_cloudflare_credentials(endpoint, &token).await
                && let Some(relay) = Self::first_available(credentials).await
            {
                return Some(relay);
            }
            tracing::debug!("Managed TURN unavailable; trying the configured fallback");
        }

        Self::first_available(vec![TurnCredentials {
            server: std::env::var("VMAX_TURN_SERVER").ok()?,
            username: std::env::var("VMAX_TURN_USERNAME").ok()?,
            password: std::env::var("VMAX_TURN_PASSWORD").ok()?,
        }])
        .await
    }

    async fn first_available(credentials: Vec<TurnCredentials>) -> Option<Self> {
        stream::iter(credentials)
            .map(|credentials| async move {
                timeout(
                    ALLOCATION_TIMEOUT,
                    Self::allocate(
                        credentials.server,
                        credentials.username,
                        credentials.password,
                    ),
                )
                .await
                .ok()?
                .ok()
            })
            .buffer_unordered(MAX_TURN_ENDPOINTS)
            .filter_map(std::future::ready)
            .next()
            .await
    }

    async fn allocate(server: String, username: String, password: String) -> io::Result<Self> {
        let conn = UdpSocket::bind("0.0.0.0:0").await?;
        crate::stun::tune_media_buffers(&socket2::SockRef::from(&conn))?;
        Self::allocate_on(server, username, password, Arc::new(conn)).await
    }

    async fn allocate_on(
        server: String,
        username: String,
        password: String,
        conn: Arc<dyn Conn + Send + Sync>,
    ) -> io::Result<Self> {
        let client = Client::new(ClientConfig {
            stun_serv_addr: server.clone(),
            turn_serv_addr: server,
            username,
            password,
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn,
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
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

    #[test]
    fn cloudflare_credentials_use_only_supported_udp_addresses() {
        assert!(cloudflare_endpoint("../invalid").is_none());
        assert_eq!(
            cloudflare_endpoint("test-key").unwrap().host_str(),
            Some("rtc.live.cloudflare.com")
        );
        let credentials = serde_json::from_value::<IceServers>(json!({
            "iceServers": [
                {"urls": "stun:stun.example.test:3478"},
                {"urls": [
                    "turns:turn.example.test:5349?transport=tcp",
                    "turn:turn.example.test:3478?transport=tcp",
                    "turn:turn.example.test",
                    "turn:turn.example.test:3478?transport=udp",
                    "turn:turn.example.test:53?transport=udp",
                    "turn:invalid@turn.example.test:3478",
                ], "username": "viewer", "credential": "temporary"},
                {"urls": "turn:192.0.2.1:3478", "username": "", "credential": "invalid"},
            ]
        }))
        .unwrap()
        .udp_credentials();
        assert_eq!(credentials.len(), 2);
        assert_eq!(credentials[0].server, "turn.example.test:3478");
        assert_eq!(credentials[1].server, "turn.example.test:53");
    }

    #[tokio::test]
    async fn managed_credentials_authenticate_and_carry_media_on_a_real_turn_allocation() {
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
            let idle_turn = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let idle_turn_addr = idle_turn.local_addr().unwrap();
            let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = Url::parse(&format!(
                "http://{}/credentials",
                http.local_addr().unwrap()
            ))
            .unwrap();
            let provider = tokio::spawn(async move {
                let (mut connection, _) = http.accept().await.unwrap();
                let mut request = Vec::new();
                BufReader::new(&mut connection)
                    .read_until(b'}', &mut request)
                    .await
                    .unwrap();
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("POST /credentials HTTP/1.1\r\n"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("\r\nauthorization: bearer token\r\n")
                );
                assert!(request.ends_with(r#"{"ttl":86400}"#));
                let body = json!({"iceServers": [{
                    "urls": [format!("turn:{idle_turn_addr}"), format!("turn:{server_addr}")],
                    "username": "viewer",
                    "credential": "test-password"
                }]});
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                connection.write_all(response.as_bytes()).await.unwrap();
            });
            let credentials = fetch_cloudflare_credentials(endpoint, "token")
                .await
                .unwrap();
            provider.await.unwrap();
            let relay = TurnRelay::first_available(credentials).await.unwrap();
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
