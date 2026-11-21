use std::sync::{Arc, Mutex};

use bytes::Bytes;
use iroh::{
    Endpoint, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};

use crate::protocol::InputPacket;

use super::{
    STREAM_ALPN,
    control::{self, StreamSettings},
    input::{self, InputInbox},
    media::MediaFanout,
};

pub(crate) const MAX_PLAYERS: usize = 4;
pub(crate) const HOST_FULL_CODE: u32 = 403;
pub(crate) const HOST_FULL_REASON: &[u8] = b"Host is full (4/4)";

pub(crate) const PLAYER_PRESENCE_EVENT: &str = "vmax://player-presence";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PlayerPresence {
    pub slot: usize,
    pub joined: bool,
}

type PlayerSlots = Mutex<[Option<EndpointId>; MAX_PLAYERS]>;

#[derive(Clone, Debug)]
pub(crate) struct StreamHost {
    permits: Arc<Semaphore>,
    slots: Arc<PlayerSlots>,
    media: MediaFanout,
    input: InputInbox,
    settings: StreamSettings,
    presence: broadcast::Sender<PlayerPresence>,
}

impl StreamHost {
    pub(crate) fn new(settings: StreamSettings) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_PLAYERS)),
            slots: Arc::new(Mutex::new([None; MAX_PLAYERS])),
            media: MediaFanout::default(),
            input: InputInbox::default(),
            settings,
            presence: broadcast::channel(32).0,
        }
    }

    pub(crate) fn mount(self, endpoint: Endpoint) -> Router {
        Router::builder(endpoint).accept(STREAM_ALPN, self).spawn()
    }

    /// Ceiling for the existing video packetizer to fit all datagram-capable peers.
    pub(crate) fn media_datagram_size(&self) -> Option<usize> {
        self.media.max_datagram_size()
    }

    /// Budget for the shared encoder. A lower budget preserves aggregate upload headroom.
    pub(crate) fn per_player_bitrate(&self, total_upload_bps: u32) -> u32 {
        total_upload_bps / self.media.active_players().max(1) as u32
    }

    /// Fans out one frame after the existing video packetizer has split it into datagrams.
    pub(crate) fn broadcast_video(&self, packets: &[Bytes]) -> usize {
        self.media.broadcast(packets)
    }

    /// Fans out one encoded audio datagram.
    pub(crate) fn broadcast_audio(&self, packet: Bytes) -> usize {
        self.media.broadcast(&[packet])
    }

    pub(crate) fn subscribe_presence(&self) -> broadcast::Receiver<PlayerPresence> {
        self.presence.subscribe()
    }

    pub(crate) fn try_input(&self, slot: usize) -> Option<InputPacket> {
        self.input.try_recv(slot)
    }

    fn reserve_slot(&self, connection: &Connection) -> Option<PlayerSlot> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        let mut slots = self.slots.lock().unwrap_or_else(|err| err.into_inner());
        let index = slots.iter().position(Option::is_none)?;
        slots[index] = Some(connection.remote_id());
        Some(PlayerSlot {
            slots: Arc::clone(&self.slots),
            index,
            media: self.media.clone(),
            input: self.input.clone(),
            presence: self.presence.clone(),
            joined: false,
            _permit: permit,
        })
    }
}

struct PlayerSlot {
    slots: Arc<PlayerSlots>,
    index: usize,
    media: MediaFanout,
    input: InputInbox,
    presence: broadcast::Sender<PlayerPresence>,
    joined: bool,
    _permit: OwnedSemaphorePermit,
}

impl Drop for PlayerSlot {
    fn drop(&mut self) {
        // Reclaim the index before the owned permit is released.
        let mut slots = self.slots.lock().unwrap_or_else(|err| err.into_inner());
        self.media.set(self.index, None);
        self.input.clear(self.index);
        slots[self.index] = None;
        if self.joined {
            let _ = self.presence.send(PlayerPresence {
                slot: self.index,
                joined: false,
            });
        }
    }
}

impl ProtocolHandler for StreamHost {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Some(mut slot) = self.reserve_slot(&connection) else {
            connection.close(HOST_FULL_CODE.into(), HOST_FULL_REASON);
            return Ok(());
        };

        let result = tokio::select! {
            _ = connection.closed() => return Ok(()),
            result = async {
                let (mut send, recv) = control::accept(&connection).await?;
                control::welcome(&mut send, slot.index, self.settings).await?;
                let inbox = self.input.register(slot.index);
                self.media.set(slot.index, Some(connection.clone()));
                slot.joined = true;
                let _ = self.presence.send(PlayerPresence { slot: slot.index, joined: true });
                tokio::select! {
                    result = control::serve(send, recv) => result,
                    result = input::receive(&connection, inbox) => result.map_err(anyhow::Error::from),
                }
            } => result,
        };
        if let Err(error) = result
            && connection.close_reason().is_none()
        {
            connection.close(400u32.into(), b"Control channel closed or invalid");
            return Err(AcceptError::from_boxed(error.into_boxed_dyn_error()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use iroh::{
        EndpointAddr,
        endpoint::{ConnectionError, presets},
    };
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::net::client::{JoinError, Route, StreamClient};
    use iroh_tickets::{Ticket, endpoint::EndpointTicket};

    #[tokio::test]
    async fn four_players_receive_media_and_disconnected_slot_is_reused() -> anyhow::Result<()> {
        timeout(Duration::from_secs(15), async {
            let endpoint = Endpoint::bind(presets::Minimal).await?;
            let port = endpoint
                .bound_sockets()
                .into_iter()
                .find(SocketAddr::is_ipv4)
                .ok_or_else(|| anyhow::anyhow!("Iroh did not bind IPv4"))?
                .port();
            let addr = EndpointAddr::new(endpoint.id()).with_ip_addr(([127, 0, 0, 1], port).into());
            let invite = EndpointTicket::new(addr.clone()).encode_string();
            let host = StreamHost::new(StreamSettings {
                width: 1920,
                height: 1080,
                fps: 60,
            });
            let mut presence = host.subscribe_presence();
            let router = host.clone().mount(endpoint);
            let mut players = Vec::new();
            assert_eq!(host.per_player_bitrate(20_000_000), 20_000_000);
            for index in 0..MAX_PLAYERS {
                let client = Endpoint::bind(presets::Minimal).await?;
                let connection = client.connect(addr.clone(), STREAM_ALPN).await?;
                let streams = check_control(&connection, index).await?;
                wait_for_slot(&host, index, Some(client.id())).await;
                assert_eq!(
                    presence.recv().await?,
                    PlayerPresence {
                        slot: index,
                        joined: true
                    }
                );
                players.push((client, connection, streams));
            }

            assert_eq!(host.per_player_bitrate(20_000_000), 5_000_000);
            assert!(host.media_datagram_size().is_some());
            let oversized = Bytes::from(vec![0; 65_536]);
            assert_eq!(host.broadcast_video(&[oversized]), 0);
            let video = [
                Bytes::from_static(b"video-1"),
                Bytes::from_static(b"video-2"),
            ];
            let audio = Bytes::from_static(b"audio");
            assert_eq!(host.broadcast_video(&video), MAX_PLAYERS);
            assert_eq!(host.broadcast_audio(audio.clone()), MAX_PLAYERS);
            for (_, connection, _) in &players {
                let mut received = [
                    connection.read_datagram().await?,
                    connection.read_datagram().await?,
                    connection.read_datagram().await?,
                ];
                received.sort_unstable();
                assert_eq!(
                    received,
                    [audio.clone(), video[0].clone(), video[1].clone()]
                );
            }

            let input = InputPacket {
                seq: 7,
                client_ts_us: 100,
                event: crate::protocol::InputEvent::MouseMoveRel { dx: 4, dy: -2 },
            };
            players[1].1.send_datagram(Bytes::from_static(b"invalid"))?;
            players[1].1.send_datagram(input.encode())?;
            loop {
                if let Some(received) = host.try_input(1) {
                    assert_eq!(received, input);
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            assert!(host.try_input(0).is_none());

            let client = Endpoint::bind(presets::Minimal).await?;
            let rejection = match client.connect(addr.clone(), STREAM_ALPN).await {
                Ok(connection) => connection.closed().await,
                Err(error) => return Err(error.into()),
            };
            let ConnectionError::ApplicationClosed(close) = rejection else {
                anyhow::bail!("expected application close, got {rejection:?}");
            };
            assert_eq!(close.error_code, HOST_FULL_CODE.into());
            assert_eq!(close.reason.as_ref(), HOST_FULL_REASON);
            assert!(matches!(
                StreamClient::connect(&client, &invite).await,
                Err(JoinError::HostFull)
            ));

            assert!(matches!(
                presence.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
            let (departed, connection, _streams) = players.remove(1);
            connection.close(0u32.into(), b"leaving");
            departed.close().await;
            wait_for_slot(&host, 1, None).await;
            assert_eq!(
                presence.recv().await?,
                PlayerPresence {
                    slot: 1,
                    joined: false
                }
            );
            assert_eq!(host.per_player_bitrate(20_000_000), 6_666_666);
            let replacement = StreamClient::connect(&client, &invite).await?;
            assert_eq!(replacement.slot, 1);
            assert_eq!(
                presence.recv().await?,
                PlayerPresence {
                    slot: 1,
                    joined: true
                }
            );
            assert_eq!(replacement.settings, host.settings);
            loop {
                let diagnostics = replacement.diagnostics();
                if diagnostics.route == Route::Direct {
                    assert!(
                        diagnostics
                            .rtt_ms
                            .is_some_and(|rtt| rtt.is_finite() && rtt > 0.0)
                    );
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            assert!(host.try_input(1).is_none());
            wait_for_slot(&host, 1, Some(client.id())).await;
            replacement.send_input(&input)?;
            loop {
                if let Some(received) = host.try_input(1) {
                    assert_eq!(received, input);
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            host.broadcast_audio(audio.clone());
            assert_eq!(replacement.read_media().await?, audio);
            for (index, (peer, _, _)) in [0, 2, 3].into_iter().zip(&players) {
                assert_eq!(host.slots.lock().unwrap()[index], Some(peer.id()));
            }
            router.shutdown().await?;
            assert_eq!(*host.slots.lock().unwrap(), [None; MAX_PLAYERS]);
            assert_eq!(host.media_datagram_size(), None);
            drop(replacement);
            Ok(())
        })
        .await?
    }

    async fn check_control(
        connection: &Connection,
        slot: usize,
    ) -> anyhow::Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        let (mut send, mut recv) = connection.open_bi().await?;
        send.write_all(&[control::HELLO]).await?;
        let mut welcome = [0; 8];
        recv.read_exact(&mut welcome).await?;
        assert_eq!(
            welcome,
            [control::WELCOME, slot as u8, 7, 128, 4, 56, 0, 60]
        );
        send.write_all(&[control::PING, 0, 0, 0, 0, 0, 0, 0, 42])
            .await?;
        let mut pong = [0; 9];
        recv.read_exact(&mut pong).await?;
        assert_eq!(pong, [control::PONG, 0, 0, 0, 0, 0, 0, 0, 42]);
        Ok((send, recv))
    }

    async fn wait_for_slot(host: &StreamHost, index: usize, peer: Option<EndpointId>) {
        while host.slots.lock().unwrap()[index] != peer {
            sleep(Duration::from_millis(5)).await;
        }
    }
}
