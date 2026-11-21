use crate::regional_wire::{MAX_SIGNAL_FRAME, ROOM_CODE_LENGTH, Reply, Request, TurnGrant};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use rand::{Rng, distr::Alphanumeric};
use ring::hmac;
use std::{
    collections::{HashMap, hash_map::Entry},
    io,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot},
    time::{interval, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request as UpgradeRequest, Response},
        http::StatusCode,
        protocol::WebSocketConfig,
    },
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;
type Socket = WebSocketStream<TcpStream>;

const MAX_CONNECTIONS: usize = 256;
const MAX_ROOMS: usize = 32;
const MAX_PENDING_PER_ROOM: usize = 4;
const INITIAL_TIMEOUT: Duration = Duration::from_secs(5);
const PAIR_TIMEOUT: Duration = Duration::from_secs(12);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const LEASE_HEARTBEAT: Duration = Duration::from_secs(15);
const MIN_TURN_TTL: u64 = 900;
const MAX_TURN_TTL: u64 = 21_600;

struct Config {
    host_verifier: hmac::Key,
    host_tag: hmac::Tag,
    turn_key: hmac::Key,
    turn_domain: String,
    turn_udp_port: u16,
    turn_tls_port: u16,
    ttl: u64,
}

impl Config {
    fn load() -> Result<Self> {
        let secret = read_secret("VMAX_TURN_SECRET_FILE")?;
        let token = read_secret("VMAX_REGIONAL_TOKEN_FILE")?;
        let domain = std::env::var("VMAX_TURN_DOMAIN")?;
        let ttl = env_number("VMAX_TURN_TTL_SECONDS", 14_400)?;
        let udp = env_number("VMAX_TURN_UDP_PORT", 3478)?;
        let tls = env_number("VMAX_TURN_TLS_PORT", 443)?;
        Self::new(&secret, &token, domain, udp, tls, ttl)
    }

    fn new(
        secret: &str,
        token: &str,
        domain: String,
        udp: u16,
        tls: u16,
        ttl: u64,
    ) -> Result<Self> {
        if secret.len() < 32 || token.len() < 32 || secret == token {
            return Err(invalid(
                "TURN secret and host token must be distinct and at least 32 characters",
            ));
        }
        if !matches!(url::Host::parse(&domain), Ok(url::Host::Domain(_)))
            || udp == 0
            || tls == 0
            || !(MIN_TURN_TTL..=MAX_TURN_TTL).contains(&ttl)
        {
            return Err(invalid("Invalid TURN domain, port, or credential lifetime"));
        }
        let host_verifier = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let host_tag = hmac::sign(&host_verifier, token.as_bytes());
        Ok(Self {
            host_verifier,
            host_tag,
            turn_key: hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes()),
            turn_domain: domain,
            turn_udp_port: udp,
            turn_tls_port: tls,
            ttl,
        })
    }

    fn accepts_host(&self, token: &str) -> bool {
        hmac::verify(
            &self.host_verifier,
            token.as_bytes(),
            self.host_tag.as_ref(),
        )
        .is_ok()
    }

    fn credentials(&self, room: &str, role: &str) -> TurnGrant {
        let expires_at = unix_seconds() + self.ttl;
        let username = format!("{expires_at}:{room}.{role}.{}", random_token(16));
        let password = STANDARD.encode(hmac::sign(&self.turn_key, username.as_bytes()).as_ref());
        TurnGrant {
            username,
            password,
            expires_at,
            domain: self.turn_domain.clone(),
            udp_port: self.turn_udp_port,
            tls_port: self.turn_tls_port,
        }
    }
}

fn read_secret(var: &str) -> Result<String> {
    Ok(std::fs::read_to_string(std::env::var(var)?)?
        .trim()
        .to_owned())
}

fn env_number<T: std::str::FromStr>(var: &str, default: T) -> Result<T> {
    match std::env::var(var) {
        Ok(value) => value.parse().map_err(|_| invalid(format!("Invalid {var}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn invalid(message: impl Into<String>) -> Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_token(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

struct Room {
    lease: String,
    invite: String,
    pending: HashMap<String, oneshot::Sender<Socket>>,
}

struct RoomLease {
    rooms: Rooms,
    code: String,
}
impl Drop for RoomLease {
    fn drop(&mut self) {
        self.rooms.lock().unwrap().remove(&self.code);
    }
}

struct PairLease {
    rooms: Rooms,
    room: String,
    pair: String,
}
impl Drop for PairLease {
    fn drop(&mut self) {
        if let Some(room) = self.rooms.lock().unwrap().get_mut(&self.room) {
            room.pending.remove(&self.pair);
        }
    }
}

pub async fn run(bind: &str) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::load().map_err(|error| io::Error::other(error.to_string()))?);
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "VmaX regional signaling ready");
    serve(listener, config).await?;
    Ok(())
}

async fn serve(listener: TcpListener, config: Arc<Config>) -> io::Result<()> {
    let rooms = Rooms::default();
    let budget = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (conn, _) = listener.accept().await?;
        let Ok(permit) = budget.clone().try_acquire_owned() else {
            continue;
        };
        let config = config.clone();
        let rooms = rooms.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle(conn, rooms, config).await {
                tracing::debug!(%error, "Regional socket closed");
            }
        });
    }
}

async fn handle(conn: TcpStream, rooms: Rooms, config: Arc<Config>) -> Result<()> {
    conn.set_nodelay(true)?;
    let mut ws = timeout(
        INITIAL_TIMEOUT,
        accept_hdr_async_with_config(
            conn,
            |request: &UpgradeRequest, response: Response| {
                if request.uri().path() == "/v1" && request.uri().query().is_none() {
                    Ok(response)
                } else {
                    let mut error = ErrorResponse::new(None);
                    *error.status_mut() = StatusCode::NOT_FOUND;
                    Err(error)
                }
            },
            Some(
                WebSocketConfig::default()
                    .max_message_size(Some(MAX_SIGNAL_FRAME))
                    .max_frame_size(Some(MAX_SIGNAL_FRAME))
                    .max_write_buffer_size(MAX_SIGNAL_FRAME * 4),
            ),
        ),
    )
    .await??;
    let message = timeout(INITIAL_TIMEOUT, ws.next())
        .await?
        .ok_or_else(|| invalid("Missing operation"))??;
    let Message::Text(text) = message else {
        return Err(invalid("Expected a signaling request"));
    };
    match serde_json::from_str::<Request>(&text)? {
        Request::Probe => reply(&mut ws, Reply::Ready).await?,
        Request::Register { host_token, invite } => {
            if !config.accepts_host(&host_token) {
                return reject(&mut ws, "Regional host token was rejected").await;
            }
            if !valid_invite(&invite) {
                return reject(&mut ws, "Invalid VmaX invitation").await;
            }
            return register(ws, rooms, invite).await;
        }
        Request::Lookup { room } => {
            let invite = rooms
                .lock()
                .unwrap()
                .get(&room.to_ascii_uppercase())
                .map(|entry| entry.invite.clone());
            return match invite {
                Some(invite) => reply(&mut ws, Reply::Invite { invite }).await,
                None => reject(&mut ws, "Regional room expired or was not found").await,
            };
        }
        Request::PairHost { room, lease, pair } => {
            return pair_host(ws, rooms, config, room, lease, pair).await;
        }
        Request::PairViewer { room, pair } => return pair_viewer(ws, rooms, &room, &pair).await,
    };
    Ok(())
}

fn valid_invite(invite: &str) -> bool {
    invite.len() <= 2048
        && url::Url::parse(invite).is_ok_and(|url| {
            url.scheme() == "vmax"
                && url.host_str() == Some("join")
                && url.path() == "/auto"
                && url
                    .query_pairs()
                    .any(|(key, value)| key == "ticket" && !value.is_empty())
                && url
                    .query_pairs()
                    .any(|(key, value)| key == "key" && value.len() == 32)
        })
}

async fn register(mut ws: Socket, rooms: Rooms, invite: String) -> Result<()> {
    let lease = random_token(32);
    let code = {
        let mut active = rooms.lock().unwrap();
        if active.len() >= MAX_ROOMS {
            return Err(invalid("Regional host limit reached"));
        }
        loop {
            let code = random_token(ROOM_CODE_LENGTH).to_ascii_uppercase();
            if let Entry::Vacant(entry) = active.entry(code.clone()) {
                entry.insert(Room {
                    lease: lease.clone(),
                    invite,
                    pending: HashMap::new(),
                });
                break code;
            }
        }
    };
    let _registration = RoomLease {
        rooms,
        code: code.clone(),
    };
    reply(&mut ws, Reply::Registered { room: code, lease }).await?;
    let mut tick = interval(LEASE_HEARTBEAT);
    let mut seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if seen.elapsed() > LEASE_HEARTBEAT * 3 { break; }
                send(&mut ws, Message::Ping(Default::default())).await?;
            }
            frame = ws.next() => match frame {
                Some(Ok(Message::Pong(_) | Message::Ping(_))) => seen = tokio::time::Instant::now(),
                _ => break,
            }
        }
    }
    Ok(())
}

async fn pair_host(
    mut ws: Socket,
    rooms: Rooms,
    config: Arc<Config>,
    code: String,
    lease: String,
    pair: String,
) -> Result<()> {
    if pair.len() != 32 || !pair.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return reject(&mut ws, "Invalid pair nonce").await;
    }
    let receiver = {
        let mut active = rooms.lock().unwrap();
        let room = active
            .get_mut(&code)
            .ok_or_else(|| invalid("Room expired"))?;
        if room.lease != lease || room.pending.len() >= MAX_PENDING_PER_ROOM {
            return Err(invalid("Pair authorization failed"));
        }
        let Entry::Vacant(entry) = room.pending.entry(pair.clone()) else {
            return Err(invalid("Pair already registered"));
        };
        let (sender, rx) = oneshot::channel();
        entry.insert(sender);
        rx
    };
    let _pending = PairLease {
        rooms,
        room: code.clone(),
        pair,
    };
    let mut peer = timeout(PAIR_TIMEOUT, receiver).await??;
    reply(
        &mut ws,
        Reply::Paired {
            turn: config.credentials(&code, "h"),
        },
    )
    .await?;
    reply(
        &mut peer,
        Reply::Paired {
            turn: config.credentials(&code, "v"),
        },
    )
    .await?;
    timeout(EXCHANGE_TIMEOUT, bridge(ws, peer)).await?
}

async fn pair_viewer(mut ws: Socket, rooms: Rooms, code: &str, pair: &str) -> Result<()> {
    // The two sockets can arrive in either order. Only the host lease can create a grant.
    let sender = timeout(INITIAL_TIMEOUT, async {
        loop {
            {
                let mut active = rooms.lock().unwrap();
                let room = active.get_mut(code)?;
                if let Some(sender) = room.pending.remove(pair) {
                    return Some(sender);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .ok()
    .flatten();
    if let Some(sender) = sender {
        let _ = sender.send(ws);
        Ok(())
    } else {
        reject(&mut ws, "The host did not authorize this peer").await
    }
}

async fn bridge(mut host: Socket, mut viewer: Socket) -> Result<()> {
    loop {
        let (incoming, peer) = tokio::select! {
            msg = host.next() => (msg, &mut viewer),
            msg = viewer.next() => (msg, &mut host),
        };
        match incoming.transpose()? {
            Some(message @ Message::Binary(_)) => send(peer, message).await?,
            Some(Message::Ping(_) | Message::Pong(_)) => {}
            _ => return Ok(()),
        }
    }
}

async fn send(socket: &mut Socket, message: Message) -> Result<()> {
    timeout(INITIAL_TIMEOUT, socket.send(message)).await??;
    Ok(())
}

async fn reply(socket: &mut Socket, value: Reply) -> Result<()> {
    send(socket, Message::Text(serde_json::to_string(&value)?.into())).await
}

async fn reject(socket: &mut Socket, message: &str) -> Result<()> {
    reply(
        socket,
        Reply::Error {
            message: message.to_owned(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{MaybeTlsStream, connect_async};

    type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;
    const TURN_SECRET: &str = "local-test-secret-only-0123456789";
    const HOST_TOKEN: &str = "local-test-token-only-abcdefghij";
    const INVITE: &str = "vmax://join/auto?ticket=local-test&key=0123456789abcdef0123456789abcdef";

    async fn connect(url: &str, request: Request) -> (Client, Reply) {
        let (mut ws, _) = connect_async(url).await.unwrap();
        ws.send(Message::Text(
            serde_json::to_string(&request).unwrap().into(),
        ))
        .await
        .unwrap();
        let Message::Text(raw) = next(&mut ws).await else {
            panic!("expected a reply")
        };
        (ws, serde_json::from_str(&raw).unwrap())
    }

    async fn next(ws: &mut Client) -> Message {
        timeout(Duration::from_secs(3), async {
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                if !matches!(msg, Message::Ping(_) | Message::Pong(_)) {
                    return msg;
                }
                ws.flush().await.unwrap();
            }
        })
        .await
        .expect("signaling test timed out")
    }

    #[tokio::test]
    async fn leases_authorize_grants_and_pair_only_opaque_messages() {
        let config = Config::new(
            TURN_SECRET,
            HOST_TOKEN,
            "turn.example.net".into(),
            3478,
            443,
            900,
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(serve(listener, Arc::new(config)));

        let register = |host_token: &str| Request::Register {
            host_token: host_token.to_owned(),
            invite: INVITE.to_owned(),
        };
        assert!(matches!(
            connect(&url, register("bad token")).await.1,
            Reply::Error { .. }
        ));
        let (mut room_ws, Reply::Registered { room, lease }) =
            connect(&url, register(HOST_TOKEN)).await
        else {
            panic!("room registration failed")
        };
        let lookup = || Request::Lookup {
            room: room.to_ascii_lowercase(),
        };
        assert!(
            matches!(connect(&url, lookup()).await.1, Reply::Invite { invite } if invite == INVITE)
        );
        assert!(matches!(
            connect(
                &url,
                Request::PairViewer {
                    room: "MISSINGROOM12345".into(),
                    pair: random_token(32),
                }
            )
            .await
            .1,
            Reply::Error { .. }
        ));

        let pair = random_token(32);
        let before = unix_seconds();
        let ((mut host, host_reply), (mut viewer, viewer_reply)) = tokio::join!(
            connect(
                &url,
                Request::PairHost {
                    room: room.clone(),
                    lease,
                    pair: pair.clone()
                }
            ),
            connect(
                &url,
                Request::PairViewer {
                    room: room.clone(),
                    pair
                }
            ),
        );
        let (Reply::Paired { turn: h }, Reply::Paired { turn: v }) = (host_reply, viewer_reply)
        else {
            panic!("both peers should receive TURN grants")
        };
        assert_ne!(h.username, v.username);
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, TURN_SECRET.as_bytes());
        for grant in [h, v] {
            assert_eq!(grant.domain, "turn.example.net");
            assert_eq!((grant.udp_port, grant.tls_port), (3478, 443));
            assert!((before + 900..=unix_seconds() + 900).contains(&grant.expires_at));
            assert_eq!(
                grant.username.split_once(':').unwrap().0,
                grant.expires_at.to_string()
            );
            assert!(
                hmac::verify(
                    &key,
                    grant.username.as_bytes(),
                    &STANDARD.decode(grant.password).unwrap()
                )
                .is_ok()
            );
        }
        let packet = Message::Binary(vec![1, 2, 3].into());
        host.send(packet.clone()).await.unwrap();
        assert_eq!(next(&mut viewer).await, packet);
        viewer.send(packet.clone()).await.unwrap();
        assert_eq!(next(&mut host).await, packet);
        room_ws.close(None).await.unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(connect(&url, lookup()).await.1, Reply::Error { .. }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("registration should expire when host disconnects");
        task.abort();
    }
}
