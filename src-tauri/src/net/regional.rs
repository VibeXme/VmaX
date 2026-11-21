//! Optional regional signaling. Messages between admitted peers are sealed
//! with keys exported from their authenticated Iroh connection.
use super::regional_wire::{MAX_SIGNAL_FRAME, ROOM_CODE_LENGTH, Reply, Request, TurnGrant};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use iroh::endpoint::Connection;
use ring::aead::{self, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use tokio::{net::TcpStream, task::JoinHandle, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use url::Url;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const TIMEOUT: Duration = Duration::from_secs(5);
const LABEL: &[u8] = b"vmax/regional-signal/1";

pub(crate) fn endpoint(input: &str) -> Option<Url> {
    let url = Url::parse(input.trim()).ok()?;
    let secure = url.scheme() == "wss"
        || (cfg!(test) && url.scheme() == "ws" && url.host_str() == Some("127.0.0.1"));
    (secure
        && url.path() == "/v1"
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

pub(crate) fn is_code(input: &str) -> bool {
    let value = input.trim();
    value.len() == ROOM_CODE_LENGTH && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

async fn connect(url: &Url, request: Request) -> Result<(Socket, Reply)> {
    crate::signaling::ensure_rustls_provider();
    timeout(TIMEOUT, async {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SIGNAL_FRAME))
            .max_frame_size(Some(MAX_SIGNAL_FRAME))
            .max_write_buffer_size(MAX_SIGNAL_FRAME * 4);
        let (mut ws, _) = connect_async_with_config(url.as_str(), Some(config), true).await?;
        ws.send(Message::Text(serde_json::to_string(&request)?.into()))
            .await?;
        let ReplyFrame::Text(reply) = receive(&mut ws).await? else {
            bail!("Expected a regional signaling reply");
        };
        let reply = serde_json::from_str(&reply)?;
        if let Reply::Error { message } = reply {
            bail!("{message}");
        }
        Ok((ws, reply))
    })
    .await
    .context("Regional signaling timed out")?
}

pub(crate) async fn lookup(url: &Url, code: &str) -> Result<String> {
    let (_, Reply::Invite { invite }) = connect(
        url,
        Request::Lookup {
            room: code.trim().to_ascii_uppercase(),
        },
    )
    .await?
    else {
        bail!("Unexpected regional room reply");
    };
    Ok(invite)
}

pub(crate) async fn probe(url: &Url) -> Result<()> {
    let (_, Reply::Ready) = connect(url, Request::Probe).await? else {
        bail!("Unexpected regional probe reply");
    };
    Ok(())
}

pub(crate) struct Room {
    pub(crate) url: Url,
    pub(crate) code: String,
    lease: String,
    heartbeat: JoinHandle<()>,
}

impl Room {
    pub(crate) async fn register(url: Url, invite: String) -> Result<Self> {
        let host_token = std::env::var("VMAX_REGIONAL_HOST_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .context("Set VMAX_REGIONAL_HOST_TOKEN on the computer that hosts regional sessions")?;
        let (mut ws, Reply::Registered { room, lease }) =
            connect(&url, Request::Register { host_token, invite }).await?
        else {
            bail!("Regional signaling did not register the room");
        };
        ensure!(
            is_code(&room) && lease.len() == 32,
            "Invalid regional room response"
        );
        let heartbeat = tokio::spawn(async move {
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Ping(payload) => {
                        if ws.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    _ => break,
                }
            }
        });
        Ok(Self {
            url,
            code: room,
            lease,
            heartbeat,
        })
    }

    pub(crate) async fn pair(&self, carrier: &Connection, pair: String) -> Result<Exchange> {
        Exchange::open(
            &self.url,
            carrier,
            Request::PairHost {
                room: self.code.clone(),
                lease: self.lease.clone(),
                pair,
            },
            true,
        )
        .await
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

pub(crate) struct Exchange {
    ws: Socket,
    tx: Cipher,
    rx: Cipher,
    pub(crate) turn: TurnGrant,
}

impl Exchange {
    pub(crate) async fn viewer(
        url: &Url,
        carrier: &Connection,
        room: String,
        pair: String,
    ) -> Result<Self> {
        Exchange::open(url, carrier, Request::PairViewer { room, pair }, false).await
    }

    async fn open(url: &Url, carrier: &Connection, request: Request, host: bool) -> Result<Self> {
        let mut key = [0; 32];
        carrier
            .export_keying_material(&mut key, LABEL, b"")
            .map_err(|_| anyhow::anyhow!("Could not derive regional signaling keys"))?;
        let (ws, Reply::Paired { turn }) = connect(url, request).await? else {
            bail!("Regional peer was not paired");
        };
        Ok(Self {
            ws,
            tx: Cipher::new(&key, host)?,
            rx: Cipher::new(&key, !host)?,
            turn,
        })
    }

    pub(crate) async fn write<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() <= MAX_SIGNAL_FRAME - CHACHA20_POLY1305.tag_len(),
            "Regional signaling message is too large"
        );
        self.tx
            .key
            .seal_in_place_append_tag(self.tx.next()?, aead::Aad::empty(), &mut bytes)
            .map_err(|_| anyhow::anyhow!("Regional signaling encryption failed"))?;
        timeout(TIMEOUT, self.ws.send(Message::Binary(bytes.into()))).await??;
        Ok(())
    }

    pub(crate) async fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        let ReplyFrame::Binary(mut bytes) = timeout(TIMEOUT, receive(&mut self.ws)).await?? else {
            bail!("Expected an encrypted regional signaling message");
        };
        let data = self
            .rx
            .key
            .open_in_place(self.rx.next()?, aead::Aad::empty(), &mut bytes)
            .map_err(|_| anyhow::anyhow!("Regional signaling authentication failed"))?;
        Ok(serde_json::from_slice(data)?)
    }

    pub(crate) async fn finish(mut self) {
        // Data frames are transmitted before the close frame on the same socket.
        let _ = timeout(Duration::from_millis(250), self.ws.close(None)).await;
    }
}

struct Cipher {
    key: LessSafeKey,
    host: bool,
    sequence: std::cell::Cell<u64>,
}

impl Cipher {
    fn new(key: &[u8], host: bool) -> Result<Self> {
        let key = UnboundKey::new(&CHACHA20_POLY1305, key)
            .map_err(|_| anyhow::anyhow!("Invalid regional signaling key"))?;
        Ok(Self {
            key: LessSafeKey::new(key),
            host,
            sequence: std::cell::Cell::new(0),
        })
    }

    fn next(&self) -> Result<Nonce> {
        let current = self.sequence.get();
        self.sequence.set(
            current
                .checked_add(1)
                .context("Signaling sequence exhausted")?,
        );
        let mut nonce = [0; 12];
        nonce[0] = u8::from(self.host);
        nonce[4..].copy_from_slice(&current.to_be_bytes());
        Ok(Nonce::assume_unique_for_key(nonce))
    }
}

enum ReplyFrame {
    Text(String),
    Binary(Vec<u8>),
}

async fn receive(ws: &mut Socket) -> Result<ReplyFrame> {
    loop {
        match ws
            .next()
            .await
            .transpose()?
            .context("Regional signaling closed")?
        {
            Message::Text(message) => return Ok(ReplyFrame::Text(message.to_string())),
            Message::Binary(message) => return Ok(ReplyFrame::Binary(message.to_vec())),
            Message::Ping(payload) => ws.send(Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            _ => bail!("Unexpected regional WebSocket message"),
        }
    }
}
