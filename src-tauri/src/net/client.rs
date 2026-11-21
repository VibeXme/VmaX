use std::time::Duration;

use anyhow::{Context, ensure};
use bytes::Bytes;
use iroh::{
    Endpoint,
    endpoint::{
        ConnectError, ConnectingError, Connection, ConnectionError, RecvStream, SendStream,
    },
};
use serde::Serialize;
use tokio::{
    task::JoinHandle,
    time::{MissedTickBehavior, interval, timeout},
};

use super::{
    STREAM_ALPN,
    control::{self, StreamSettings},
    host::{HOST_FULL_CODE, MAX_PLAYERS},
    parse_host_ticket,
};
use crate::protocol::InputPacket;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum JoinError {
    #[error("Host is full (4/4)")]
    HostFull,
    #[error("{0}")]
    Failed(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    Direct,
    Relay,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ConnectionDiagnostics {
    pub route: Route,
    pub rtt_ms: Option<f64>,
}

pub(crate) struct StreamClient {
    connection: Connection,
    heartbeat: JoinHandle<()>,
    pub slot: u8,
    pub settings: StreamSettings,
}

impl StreamClient {
    pub(crate) async fn connect(endpoint: &Endpoint, invite: &str) -> Result<Self, JoinError> {
        let ticket = parse_host_ticket(invite)?;
        let connection = timeout(
            CONNECT_TIMEOUT,
            endpoint.connect(ticket.endpoint_addr().clone(), STREAM_ALPN),
        )
        .await
        .context("Timed out connecting to the VmaX host")?
        .map_err(map_connect_error)?;

        let (slot, settings, send, recv) = match open_control(&connection).await {
            Ok(welcome) => welcome,
            Err(error) => {
                if is_host_full(connection.close_reason().as_ref()) {
                    return Err(JoinError::HostFull);
                }
                connection.close(400u32.into(), b"Control handshake failed");
                return Err(JoinError::Failed(
                    error.context("VmaX control handshake failed"),
                ));
            }
        };

        let heartbeat_connection = connection.clone();
        let heartbeat = tokio::spawn(async move {
            tokio::select! {
                _ = heartbeat_connection.closed() => {}
                result = keep_alive(send, recv) => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "VmaX control heartbeat failed");
                        heartbeat_connection.close(400u32.into(), b"Control heartbeat failed");
                    }
                }
            }
        });

        Ok(Self {
            connection,
            heartbeat,
            slot,
            settings,
        })
    }

    pub(crate) fn diagnostics(&self) -> ConnectionDiagnostics {
        let paths = self.connection.paths();
        let selected = paths.iter().find(|path| path.is_selected());
        let route = match selected.as_ref() {
            Some(path) if path.is_ip() => Route::Direct,
            Some(path) if path.is_relay() => Route::Relay,
            _ => Route::Unknown,
        };
        ConnectionDiagnostics {
            route,
            rtt_ms: selected.map(|path| path.rtt().as_secs_f64() * 1_000.0),
        }
    }

    pub(crate) fn send_input(
        &self,
        packet: &InputPacket,
    ) -> Result<(), iroh::endpoint::SendDatagramError> {
        self.connection.send_datagram(packet.encode())
    }

    pub(crate) async fn read_media(&self) -> Result<Bytes, ConnectionError> {
        self.connection.read_datagram().await
    }
}

impl Drop for StreamClient {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"Client left");
        self.heartbeat.abort();
    }
}

fn is_host_full(error: Option<&ConnectionError>) -> bool {
    matches!(error, Some(ConnectionError::ApplicationClosed(close)) if close.error_code == HOST_FULL_CODE.into())
}

fn map_connect_error(error: ConnectError) -> JoinError {
    let cause = match &error {
        ConnectError::Connection { source, .. }
        | ConnectError::Connecting {
            source: ConnectingError::ConnectionError { source, .. },
            ..
        } => Some(source),
        _ => None,
    };
    if is_host_full(cause) {
        JoinError::HostFull
    } else {
        JoinError::Failed(anyhow::Error::new(error).context("Could not connect to the VmaX host"))
    }
}

async fn open_control(
    connection: &Connection,
) -> anyhow::Result<(u8, StreamSettings, SendStream, RecvStream)> {
    timeout(control::HANDSHAKE_TIMEOUT, async {
        let (mut send, mut recv) = connection.open_bi().await?;
        send.set_priority(100)?;
        send.write_all(&[control::HELLO]).await?;
        let mut message = [0; 8];
        recv.read_exact(&mut message).await?;
        let [tag, slot, w0, w1, h0, h1, f0, f1] = message;
        ensure!(
            tag == control::WELCOME && usize::from(slot) < MAX_PLAYERS,
            "invalid welcome message"
        );
        let settings = StreamSettings {
            width: u16::from_be_bytes([w0, w1]),
            height: u16::from_be_bytes([h0, h1]),
            fps: u16::from_be_bytes([f0, f1]),
        };
        Ok((slot, settings, send, recv))
    })
    .await?
}

async fn keep_alive(mut send: SendStream, mut recv: RecvStream) -> anyhow::Result<()> {
    let mut ticks = interval(HEARTBEAT_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut sequence = 0u64;
    loop {
        ticks.tick().await;
        let mut ping = [0; 9];
        ping[0] = control::PING;
        ping[1..].copy_from_slice(&sequence.to_be_bytes());
        timeout(control::HANDSHAKE_TIMEOUT, async {
            send.write_all(&ping).await?;
            let mut pong = [0; 9];
            recv.read_exact(&mut pong).await?;
            ensure!(
                pong[0] == control::PONG && pong[1..] == ping[1..],
                "invalid heartbeat response"
            );
            Ok(())
        })
        .await??;
        sequence = sequence.wrapping_add(1);
    }
}
