//! Persistent, authenticated room with separate transport and queues per viewer.
mod motion;
mod raw;
mod rtc;
mod setup;

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        Arc, Mutex as SyncMutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch};

use super::host::{HOST_FULL_CODE, HOST_FULL_REASON, MAX_PLAYERS, PlayerPresence};
use crate::{
    bitrate::{BitrateDecision, CongestionState, StreamRate, codec_floor},
    net::regional,
    network::{
        ConnectionPath, ConnectionPhase, NetworkConfig, NetworkError, NetworkEvent,
        NetworkTelemetry, OutboundVideoFrame, VideoBacklog, queue_received_video, set_phase,
    },
    protocol::{AudioPacket, ChannelId, ControlMessage, InputPacket, ReassembledFrame, VideoCodec},
};

const ALPN: &[u8] = b"vmax/hybrid/1";
const BANDWIDTH_CODE: u32 = 429;
const INVITE_PATH: &str = "/auto";
const PLI_INTERVAL: Duration = Duration::from_millis(200);
const VIDEO_SILENCE: Duration = Duration::from_secs(1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const BUFFER: usize = 32;

type Events = mpsc::UnboundedSender<NetworkEvent>;
type Telemetry = Arc<Mutex<NetworkTelemetry>>;

#[derive(Clone, Copy, Debug)]
struct Path {
    kind: ConnectionPath,
    address: Option<SocketAddr>,
    rtt: Duration,
    loss: f64,
    estimate: Option<u32>,
}

enum Incoming {
    Video(Box<ReassembledFrame>),
    Packet(Bytes),
    Keyframe,
}

/// Both transports consume the same access unit and return the same decoded
/// wire messages. A transport owns its packetization, pacing and feedback.
trait StreamTransport: Send {
    fn path(&self) -> Path;
    fn supports_main10(&self) -> bool {
        false
    }
    fn keyframe_in_flight(&self) -> bool {
        false
    }
    fn video(
        &mut self,
        frame: OutboundVideoFrame,
        rate: u32,
    ) -> impl Future<Output = Result<bool>> + Send;
    fn send(&mut self, packet: Bytes) -> impl Future<Output = Result<()>> + Send;
    fn next(&mut self) -> impl Future<Output = Result<Incoming>> + Send;
    fn take_video_packets(&mut self) -> u64;
    fn take_video_bytes(&mut self) -> u64;
    fn take_sent_packets(&mut self) -> u64;
}

#[derive(Clone)]
pub(crate) struct HostControl(Arc<HostState>);

struct HostState {
    permits: Arc<Semaphore>,
    slots: SyncMutex<[Option<Player>; MAX_PLAYERS]>,
    target: AtomicU32,
    upload: AtomicU32,
    fixed: AtomicBool,
    current_rate: AtomicU32,
    last_idr: SyncMutex<Option<Instant>>,
    strict: AtomicBool,
    codec: AtomicU32,
    backlog: VideoBacklog,
}

struct Player {
    id: EndpointId,
    active: bool,
    control: mpsc::Sender<ControlMessage>,
    rumble: watch::Sender<crate::haptics::Rumble>,
    path: Option<Path>,
    limit: Option<u32>,
    codecs: Vec<VideoCodec>,
    remote_loss: f64,
    min_rtt: Option<Duration>,
    last_relative_motion: Option<u64>,
    display: Option<crate::handshake::ClientDisplay>,
}

struct Slot {
    state: HostControl,
    index: usize,
    events: Events,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self.state.0.slots.lock().unwrap();
        if slots[self.index].take().is_some_and(|player| player.active) {
            let _ = self
                .events
                .send(NetworkEvent::PlayerPresence(PlayerPresence {
                    slot: self.index,
                    joined: false,
                }));
        }
    }
}

impl HostControl {
    pub(crate) fn new(config: &NetworkConfig) -> Self {
        Self(Arc::new(HostState {
            permits: Arc::new(Semaphore::new(MAX_PLAYERS)),
            slots: SyncMutex::default(),
            target: AtomicU32::new(config.max_bitrate_bps),
            upload: AtomicU32::new(
                config
                    .upload_budget_bps
                    .or(config.strict_upload_bps)
                    .unwrap_or(config.max_bitrate_bps),
            ),
            fixed: AtomicBool::new(config.fixed_bitrate),
            current_rate: AtomicU32::new(config.initial_bitrate_bps),
            last_idr: SyncMutex::new(None),
            strict: AtomicBool::new(config.strict_upload_bps.is_some()),
            codec: AtomicU32::new(config.host_codec.unwrap_or(VideoCodec::Hevc) as u32),
            backlog: VideoBacklog::new(),
        }))
    }

    pub(crate) fn update_budget(&self, target: u32, upload: u32, fixed: bool, strict: bool) {
        self.0.target.store(target, Ordering::Relaxed);
        self.0.upload.store(upload, Ordering::Relaxed);
        self.0.fixed.store(fixed, Ordering::Relaxed);
        self.0.strict.store(strict, Ordering::Relaxed);
    }

    fn strict_upload(&self) -> Option<u32> {
        self.0
            .strict
            .load(Ordering::Relaxed)
            .then(|| self.0.upload.load(Ordering::Relaxed))
    }

    pub(crate) fn backlog(&self) -> VideoBacklog {
        self.0.backlog.clone()
    }

    pub(crate) fn rumble(&self, slot: usize, update: crate::haptics::Rumble) {
        if let Some(player) = self
            .0
            .slots
            .lock()
            .unwrap()
            .get(slot)
            .and_then(Option::as_ref)
        {
            player.rumble.send_replace(update);
        }
    }

    pub(crate) fn control(&self, slot: usize, message: ControlMessage) {
        if let Some(player) = self
            .0
            .slots
            .lock()
            .unwrap()
            .get(slot)
            .and_then(Option::as_ref)
        {
            let _ = player.control.try_send(message);
        }
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::from_u8(self.0.codec.load(Ordering::Relaxed) as u8).unwrap_or(VideoCodec::H264)
    }
}

#[derive(Clone)]
struct Room {
    token: Arc<str>,
    state: HostControl,
    config: NetworkConfig,
    frames: watch::Receiver<Option<OutboundVideoFrame>>,
    audio: broadcast::Sender<AudioPacket>,
    events: Events,
    region: Option<Arc<regional::Room>>,
}

impl std::fmt::Debug for Room {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Room").finish_non_exhaustive()
    }
}

impl ProtocolHandler for Room {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let result = tokio::select! {
            _ = connection.closed() => return Ok(()),
            result = self.accept_peer(&connection) => result,
        };
        if let Err(error) = result {
            tracing::debug!(%error, "Hybrid viewer left");
            if error.to_string() == crate::network::BANDWIDTH_FULL {
                connection.close(
                    BANDWIDTH_CODE.into(),
                    crate::network::BANDWIDTH_FULL.as_bytes(),
                );
            } else {
                connection.close(400u32.into(), b"Session handshake or transport closed");
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Hello {
    token: String,
    codecs: Vec<VideoCodec>,
    #[serde(default)]
    regional: bool,
}

#[derive(Serialize, Deserialize)]
struct Welcome {
    slot: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<Admission>,
}

#[derive(Serialize, Deserialize)]
struct Admission {
    endpoint: String,
    room: String,
    pair: String,
}

impl Room {
    async fn accept_peer(&self, connection: &Connection) -> Result<()> {
        let (mut send, mut recv) =
            tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
        let hello: Hello = setup::read(&mut recv).await?;
        ensure!(hello.token == self.token.as_ref(), "Invalid invitation");
        let codecs = supported_codecs(hello.codecs);
        let Ok(permit) = self.state.0.permits.clone().try_acquire_owned() else {
            connection.close(HOST_FULL_CODE.into(), HOST_FULL_REASON);
            return Ok(());
        };
        let (control, control_rx) = mpsc::channel(BUFFER);
        let (rumble, rumble_rx) = watch::channel(crate::haptics::Rumble::default());
        let index = {
            let mut slots = self.state.0.slots.lock().unwrap();
            let Some(index) = slots.iter().position(Option::is_none) else {
                connection.close(HOST_FULL_CODE.into(), HOST_FULL_REASON);
                return Ok(());
            };
            let codec = if codecs.contains(&VideoCodec::Hevc)
                && self.state.codec() != VideoCodec::H264
                && slots
                    .iter()
                    .flatten()
                    .all(|player| player.codecs.contains(&VideoCodec::Hevc))
            {
                VideoCodec::Hevc
            } else {
                VideoCodec::H264
            };
            if let Some(upload) = self.state.strict_upload()
                && upload / codec_floor(codec) < slots.iter().flatten().count() as u32 + 1
            {
                connection.close(
                    BANDWIDTH_CODE.into(),
                    crate::network::BANDWIDTH_FULL.as_bytes(),
                );
                return Ok(());
            }
            let id = connection.remote_id();
            ensure!(
                !slots.iter().flatten().any(|player| player.id == id),
                "Viewer already connected"
            );
            slots[index] = Some(Player {
                id,
                active: false,
                control,
                rumble,
                path: None,
                limit: None,
                codecs,
                remote_loss: 0.0,
                min_rtt: None,
                last_relative_motion: None,
                display: None,
            });
            index
        };
        let slot = Slot {
            state: self.state.clone(),
            index,
            events: self.events.clone(),
            _permit: permit,
        };
        let region = self.region.as_ref().filter(|_| hello.regional);
        let admission = region.map(|room| Admission {
            endpoint: room.url.to_string(),
            room: room.code.clone(),
            pair: rand::rng()
                .sample_iter(Alphanumeric)
                .take(32)
                .map(char::from)
                .collect(),
        });
        let welcome = Welcome {
            slot: index,
            region: admission,
        };
        setup::write(&mut send, &welcome).await?;
        let exchange = if let (Some(region), Some(admission)) = (region, welcome.region) {
            Some(region.pair(connection, admission.pair).await)
        } else {
            None
        };
        let exchange = setup::Exchange::select(send, recv, exchange).await?;
        match setup::negotiate(connection.clone(), exchange, &self.config, true).await? {
            setup::Transport::Raw(link) => self.stream(link, slot, control_rx, rumble_rx).await,
            setup::Transport::Rtc(link) => self.stream(link, slot, control_rx, rumble_rx).await,
        }
    }

    async fn stream<T: StreamTransport>(
        &self,
        mut link: T,
        slot: Slot,
        mut control: mpsc::Receiver<ControlMessage>,
        mut rumble: watch::Receiver<crate::haptics::Rumble>,
    ) -> Result<()> {
        let index = slot.index;
        let mut video = self.frames.clone();
        video.borrow_and_update();
        let mut audio = self.audio.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.state.0.slots.lock().unwrap()[index]
            .as_mut()
            .unwrap()
            .active = true;
        let _ = self
            .events
            .send(NetworkEvent::PlayerPresence(PlayerPresence {
                slot: index,
                joined: true,
            }));
        let mut last_pli = None;
        let mut last_frame: Option<u32> = None;
        let mut waiting_for_keyframe = true;
        let mut last_seen = Instant::now();
        loop {
            tokio::select! {
                incoming = link.next() => {
                    match incoming? {
                        Incoming::Keyframe => {
                            waiting_for_keyframe = true;
                            self.request_idr(&mut last_pli);
                        }
                        Incoming::Packet(bytes) => {
                            last_seen = Instant::now();
                            self.receive(index, &mut link, bytes, &mut last_pli).await?;
                        }
                        Incoming::Video(_) => {}
                    }
                }
                Ok(()) = video.changed() => {
                    let frame = video.borrow_and_update().clone();
                    if let Some(frame) = frame {
                        self.state.0.codec.store(frame.codec as u32, Ordering::Relaxed);
                        if let Some(upload) = self.state.strict_upload() {
                            let slots = self.state.0.slots.lock().unwrap();
                            let rank = slots[..=index].iter().flatten().filter(|player| player.active).count() as u32;
                            ensure!(rank <= upload / codec_floor(frame.codec), "{}", crate::network::BANDWIDTH_FULL);
                        }
                        if !frame.is_keyframe && (waiting_for_keyframe
                            || last_frame.is_some_and(|last| last.wrapping_add(1) != frame.frame_id))
                        {
                            waiting_for_keyframe = true;
                            if !link.keyframe_in_flight() {
                                self.request_idr(&mut last_pli);
                            }
                        } else {
                            let id = frame.frame_id;
                            waiting_for_keyframe = !link.video(frame, self.rate()).await?;
                            if !waiting_for_keyframe {
                                last_frame = Some(id);
                            }
                        }
                    }
                }
                result = audio.recv() => {
                    if let Ok(packet) = result { link.send(packet.encode()).await?; }
                }
                Some(message) = control.recv() => link.send(message.encode_datagram()?).await?,
                Ok(()) = rumble.changed() => {
                    let update = *rumble.borrow_and_update();
                    link.send(update.encode()).await?;
                }
                _ = tick.tick() => {
                    ensure!(last_seen.elapsed() < IDLE_TIMEOUT, "Viewer stopped responding");
                    self.state.0.backlog.note_raw_packets(link.take_sent_packets());
                    {
                        let mut slots = self.state.0.slots.lock().unwrap();
                        let player = slots[index].as_mut().unwrap();
                        let mut path = link.path();
                        path.loss = path.loss.max(player.remote_loss);
                        if !path.rtt.is_zero() {
                            player.min_rtt = Some(player.min_rtt.map_or(path.rtt, |min| min.min(path.rtt)));
                        }
                        player.path = Some(path);
                    }
                    if waiting_for_keyframe && !link.keyframe_in_flight() {
                        self.request_idr(&mut last_pli);
                    }
                    link.send(ControlMessage::Ping { client_ts_us: 0 }.encode_datagram()?).await?;
                }
            }
        }
    }

    async fn receive<T: StreamTransport>(
        &self,
        slot: usize,
        link: &mut T,
        bytes: Bytes,
        last_pli: &mut Option<Instant>,
    ) -> Result<()> {
        match bytes.first().and_then(|tag| ChannelId::try_from(*tag).ok()) {
            Some(ChannelId::Input) => {
                let mut slots = self.state.0.slots.lock().unwrap();
                if let Some(player) = &mut slots[slot] {
                    motion::receive(&bytes, &mut player.last_relative_motion, |packet| {
                        let _ = self.events.send(NetworkEvent::PlayerInput { slot, packet });
                    });
                }
            }
            Some(ChannelId::Control) => match ControlMessage::decode_datagram(&bytes)? {
                ControlMessage::Ping { client_ts_us } => {
                    link.send(
                        ControlMessage::Pong {
                            client_ts_us,
                            peer_ts_us: 0,
                        }
                        .encode_datagram()?,
                    )
                    .await?;
                }
                ControlMessage::RequestKeyframe { .. } => {
                    if !link.keyframe_in_flight() {
                        self.request_idr(last_pli);
                    }
                }
                ControlMessage::ClientBitrateLimit { mbps } => {
                    let mut slots = self.state.0.slots.lock().unwrap();
                    slots[slot].as_mut().unwrap().limit = Some(mbps.clamp(1, 200) * 1_000_000);
                }
                ControlMessage::ClientCapabilities {
                    preferred_codecs,
                    mut display,
                } => {
                    let mut slots = self.state.0.slots.lock().unwrap();
                    let player = slots[slot].as_mut().unwrap();
                    player.codecs = supported_codecs(preferred_codecs);
                    if let Some(display) = &mut display {
                        display.hevc_main10 &=
                            link.supports_main10() && player.codecs.contains(&VideoCodec::Hevc);
                        display.hevc_444 = false;
                    }
                    player.display = display;
                }
                ControlMessage::TelemetryFeedback { loss_pct, .. }
                    if (0.0..=100.0).contains(&loss_pct) =>
                {
                    let mut slots = self.state.0.slots.lock().unwrap();
                    slots[slot].as_mut().unwrap().remote_loss = loss_pct;
                }
                ControlMessage::Disconnect { .. } => bail!("Viewer disconnected"),
                message @ ControlMessage::ReconnectGamepad => {
                    let _ = self
                        .events
                        .send(NetworkEvent::PlayerControl { slot, message });
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn request_idr(&self, last: &mut Option<Instant>) {
        if last.is_none_or(|stamp| stamp.elapsed() >= PLI_INTERVAL) {
            let now = Instant::now();
            *last = Some(now);
            let mut shared = self.state.0.last_idr.lock().unwrap();
            if shared.is_some_and(|stamp| stamp.elapsed() < PLI_INTERVAL) {
                return;
            }
            *shared = Some(now);
            let _ = self.events.send(NetworkEvent::KeyframeRequested {
                last_good_frame_id: None,
                reason: "viewer requires a keyframe".into(),
            });
        }
    }

    fn ceiling(&self, slots: &[Option<Player>; MAX_PLAYERS]) -> u32 {
        let count = slots
            .iter()
            .flatten()
            .filter(|peer| peer.active)
            .count()
            .max(1) as u32;
        let mut rate = self
            .state
            .0
            .target
            .load(Ordering::Relaxed)
            .min(self.state.0.upload.load(Ordering::Relaxed) / count);
        if let Some(limit) = slots
            .iter()
            .flatten()
            .filter(|peer| peer.active)
            .filter_map(|peer| peer.limit)
            .min()
        {
            rate = rate.min(limit);
        }
        rate
    }

    fn rate(&self) -> u32 {
        let slots = self.state.0.slots.lock().unwrap();
        self.state
            .0
            .current_rate
            .load(Ordering::Relaxed)
            .min(self.ceiling(&slots))
    }
}

fn supported_codecs(offered: Vec<VideoCodec>) -> Vec<VideoCodec> {
    let mut codecs = Vec::with_capacity(2);
    if offered.contains(&VideoCodec::Hevc) {
        codecs.push(VideoCodec::Hevc);
    }
    codecs.push(VideoCodec::H264);
    codecs
}
pub(crate) fn is_invite(invite: &str) -> bool {
    url::Url::parse(invite.trim()).is_ok_and(|url| {
        url.scheme() == "vmax" && url.host_str() == Some("join") && url.path() == INVITE_PATH
    })
}

fn hybrid_error(error: anyhow::Error) -> NetworkError {
    NetworkError::Hybrid(format!("{error:#}"))
}

pub(crate) async fn run_host(
    state: HostControl,
    config: NetworkConfig,
    frames: watch::Receiver<Option<OutboundVideoFrame>>,
    mut audio: mpsc::Receiver<AudioPacket>,
    mut controls: mpsc::Receiver<ControlMessage>,
    events: Events,
    telemetry: Telemetry,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), NetworkError> {
    let endpoint = if config.relay_only {
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .clear_ip_transports()
            .bind()
            .await
            .map_err(|e| hybrid_error(e.into()))?;
        tokio::time::timeout(Duration::from_secs(5), endpoint.online())
            .await
            .context("Could not reach the relay for relay-only hosting")
            .map_err(hybrid_error)?;
        endpoint
    } else {
        super::bind_host().await.map_err(hybrid_error)?
    };
    let mut invite = url::Url::parse("vmax://join/auto").unwrap();
    let token: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    invite
        .query_pairs_mut()
        .append_pair("ticket", &super::host_ticket(&endpoint))
        .append_pair("key", &token);
    let region = if let Some(url) = regional::endpoint(&config.signaling_url) {
        Some(Arc::new(
            regional::Room::register(url, invite.to_string())
                .await
                .map_err(hybrid_error)?,
        ))
    } else {
        None
    };
    let code = region.as_ref().map(|room| room.code.clone());
    {
        let mut snapshot = telemetry.lock().await;
        snapshot.invite_uri = Some(invite.to_string());
        snapshot.room_code = code.clone();
    }
    let _ = events.send(NetworkEvent::InviteReady {
        room_code: code.unwrap_or_default(),
        invite_uri: invite.to_string(),
    });
    set_phase(ConnectionPhase::WaitingForPeer, &telemetry, &events).await;
    let room = Room {
        token: token.into(),
        state,
        config,
        frames,
        audio: broadcast::channel(8).0,
        events: events.clone(),
        region,
    };
    let router = Router::builder(endpoint).accept(ALPN, room.clone()).spawn();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut previous = None;
    let mut previous_rate = room.config.initial_bitrate_bps;
    let mut previous_codecs = vec![];
    let mut previous_display = None;
    let mut abr = StreamRate::default();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(packet) = audio.recv() => { let _ = room.audio.send(packet); }
            Some(message) = controls.recv() => {
                for player in room.state.0.slots.lock().unwrap().iter().flatten() {
                    let _ = player.control.try_send(message.clone());
                }
            }
            _ = tick.tick() => {
                let (count, path, codecs, ceiling, congested, display) = {
                    let slots = room.state.0.slots.lock().unwrap();
                    let count = slots.iter().flatten().filter(|peer| peer.active).count();
                    let path = slots.iter().flatten().filter_map(|peer| peer.path)
                        .max_by_key(|path| path.rtt);
                    let hevc = slots.iter().flatten().filter(|peer| peer.active)
                        .all(|peer| peer.codecs.contains(&VideoCodec::Hevc));
                    let codecs = if hevc { vec![VideoCodec::Hevc, VideoCodec::H264] } else { vec![VideoCodec::H264] };
                    let ceiling = room.ceiling(&slots);
                    let congested = slots.iter().flatten().filter(|peer| peer.active).any(|peer| {
                        peer.path.is_some_and(|path| {
                            path.loss > 2.0 || peer.min_rtt.is_some_and(|min| path.rtt > min + Duration::from_millis(25))
                            || path.estimate.is_some_and(|estimate| estimate < previous_rate * 9 / 10)
                        })
                    });
                    let display = if count == 1 {
                        slots.iter().flatten().find(|peer| peer.active).and_then(|peer| peer.display)
                    } else {
                        None
                    };
                    (count, path, codecs, ceiling, congested, display)
                };
                let new_phase = if count == 0 { ConnectionPhase::WaitingForPeer } else { ConnectionPhase::Connected };
                let rate = if room.state.0.fixed.load(Ordering::Relaxed) || count == 0 { ceiling } else {
                    abr.update(Instant::now(), previous_rate, ceiling, room.state.codec(), congested)
                };
                room.state.0.current_rate.store(rate, Ordering::Relaxed);
                {
                    let mut snapshot = telemetry.lock().await;
                    snapshot.active_players = count as u8;
                    snapshot.target_bitrate_bps = rate;
                    report_path(&mut snapshot, path);
                }
                if count > 0 && (previous_codecs != codecs || previous_display != display) {
                    previous_codecs = codecs.clone();
                    previous_display = display;
                    let _ = events.send(NetworkEvent::ControlReceived(ControlMessage::ClientCapabilities { preferred_codecs: codecs, display }));
                } else if count == 0 {
                    previous_codecs.clear();
                    previous_display = None;
                }
                if rate != previous_rate {
                    previous_rate = rate;
                    let _ = events.send(NetworkEvent::BitrateUpdated(bitrate_decision(rate, path)));
                }
                if previous.replace(new_phase) != Some(new_phase) {
                    set_phase(new_phase, &telemetry, &events).await;
                } else {
                    let _ = events.send(NetworkEvent::TelemetryUpdated(Box::new(telemetry.lock().await.clone())));
                }
            }
        }
    }
    let _ = router.shutdown().await;
    set_phase(ConnectionPhase::Disconnected, &telemetry, &events).await;
    Ok(())
}

pub(crate) async fn run_client(
    config: NetworkConfig,
    invite: String,
    input: mpsc::Receiver<InputPacket>,
    control: mpsc::Receiver<ControlMessage>,
    events: Events,
    telemetry: Telemetry,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), NetworkError> {
    let result = tokio::select! {
        _ = shutdown.changed() => Ok(()),
        result = connect_client(config, &invite, input, control, &events, &telemetry) => result,
    };
    if result.is_ok() {
        set_phase(ConnectionPhase::Disconnected, &telemetry, &events).await;
    }
    result.map_err(hybrid_error)
}

async fn connect_client(
    config: NetworkConfig,
    invite: &str,
    input: mpsc::Receiver<InputPacket>,
    mut control: mpsc::Receiver<ControlMessage>,
    events: &Events,
    telemetry: &Telemetry,
) -> Result<()> {
    let url = url::Url::parse(invite.trim())?;
    let field = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let ticket = super::parse_host_ticket(&field("ticket").context("Missing host ticket")?)?;
    let token = field("key")
        .filter(|key| key.len() == 32)
        .context("Missing invitation key")?;
    set_phase(ConnectionPhase::HolePunching, telemetry, events).await;
    let endpoint = super::bind_client().await?;
    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(ticket.endpoint_addr().clone(), ALPN),
    )
    .await??;
    let (mut send, mut recv) = connection.open_bi().await?;
    let mut codecs = config.viewer_codecs.clone();
    let mut initial_controls = Vec::new();
    while let Ok(message) = control.try_recv() {
        if let ControlMessage::ClientCapabilities {
            preferred_codecs, ..
        } = &message
        {
            codecs = preferred_codecs.clone();
        }
        initial_controls.push(message);
    }
    setup::write(
        &mut send,
        &Hello {
            token,
            codecs,
            regional: true,
        },
    )
    .await?;
    let welcome: Welcome = match setup::read(&mut recv).await {
        Ok(value) => value,
        Err(error) => return Err(join_error(connection.close_reason(), error)),
    };
    ensure!(welcome.slot < MAX_PLAYERS, "Invalid player slot");
    telemetry.lock().await.player_slot = Some(welcome.slot as u8);
    let _close = ClosePeer(connection.clone());
    tokio::select! {
        biased;
        reason = connection.closed() => {
            if matches!(&reason, iroh::endpoint::ConnectionError::ApplicationClosed(close) if close.error_code == 0u32.into()) { return Ok(()); }
            Err(join_error(Some(reason), anyhow::anyhow!("Host connection closed")))
        }
        result = async {
            let region = if let Some(admission) = welcome.region {
                Some(async {
                    let url = regional::endpoint(&admission.endpoint).context("Invalid regional signaling URL")?;
                    regional::Exchange::viewer(&url, &connection, admission.room, admission.pair).await
                }.await)
            } else {
                None
            };
            let exchange = setup::Exchange::select(send, recv, region).await?;
            match setup::negotiate(connection.clone(), exchange, &config, false).await? {
                setup::Transport::Raw(link) => client_stream(link, input, control, initial_controls, events, telemetry).await,
                setup::Transport::Rtc(link) => client_stream(link, input, control, initial_controls, events, telemetry).await,
            }
        } => result.map_err(|error| join_error(connection.close_reason(), error)),
    }
}

fn join_error(
    reason: Option<iroh::endpoint::ConnectionError>,
    source: anyhow::Error,
) -> anyhow::Error {
    match reason {
        Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
            if close.error_code == HOST_FULL_CODE.into() =>
        {
            anyhow::anyhow!("Host is full (4/4)")
        }
        Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
            if close.error_code == BANDWIDTH_CODE.into() =>
        {
            anyhow::anyhow!("{}", crate::network::BANDWIDTH_FULL)
        }
        _ => source,
    }
}

struct ClosePeer(Connection);
impl Drop for ClosePeer {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"Viewer left");
    }
}

async fn client_stream<T: StreamTransport>(
    mut link: T,
    mut input: mpsc::Receiver<InputPacket>,
    mut controls: mpsc::Receiver<ControlMessage>,
    initial_controls: Vec<ControlMessage>,
    events: &Events,
    telemetry: &Telemetry,
) -> Result<()> {
    {
        let mut snapshot = telemetry.lock().await;
        report_path(&mut snapshot, Some(link.path()));
    }
    set_phase(ConnectionPhase::Connected, telemetry, events).await;
    for message in initial_controls {
        if !matches!(message, ControlMessage::RequestKeyframe { .. }) {
            link.send(message.encode_datagram()?).await?;
        }
    }
    let pending = Arc::new(SyncMutex::new(None));
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_seen = Instant::now();
    let mut last_video = last_seen;
    let mut last_pli = last_seen;
    let mut latest_frame = None;
    let mut waiting = true;
    let mut frames = 0;
    let mut dropped = 0;
    let mut packets = 0;
    let mut recovered = 0;
    let mut audio = 0;
    let mut window_bytes = 0u64;
    let mut window_start = Instant::now();
    let mut relative_motion = motion::History::default();
    let mut motion_replay: Option<(Instant, Bytes)> = None;
    loop {
        tokio::select! {
            incoming = link.next() => match incoming? {
                Incoming::Video(frame) => {
                    last_seen = Instant::now();
                    let newer = latest_frame.is_none_or(|last: u32| (frame.frame_id.wrapping_sub(last) as i32) > 0);
                    if !newer { continue; }
                    waiting |= latest_frame.is_some_and(|last: u32| frame.frame_id != last.wrapping_add(1));
                    if waiting && !frame.is_keyframe {
                        dropped += 1;
                        continue;
                    }
                    let id = frame.frame_id;
                    let keyframe = frame.is_keyframe;
                    let repaired = frame.fec_recovered_shards;
                    let overflow = queue_received_video(&pending, events, *frame);
                    dropped += u64::from(overflow);
                    waiting = overflow && !keyframe;
                    if !waiting {
                        latest_frame = Some(id);
                        last_video = last_seen;
                        frames += 1;
                        recovered += u64::from(repaired);
                    }
                }
                Incoming::Keyframe => waiting = true,
                Incoming::Packet(bytes) => {
                    last_seen = Instant::now();
                    match bytes.first().and_then(|tag| ChannelId::try_from(*tag).ok()) {
                        Some(ChannelId::Audio) => if let Ok(packet) = AudioPacket::decode(&bytes) {
                            audio += 1;
                            let _ = events.send(NetworkEvent::AudioPacketReceived(packet));
                        },
                        Some(ChannelId::Haptics) => if let Some(rumble) = crate::haptics::Rumble::decode(&bytes) {
                            let _ = events.send(NetworkEvent::RumbleReceived(rumble));
                        },
                        Some(ChannelId::Control) => match ControlMessage::decode_datagram(&bytes)? {
                            ControlMessage::Ping { client_ts_us } => link.send(ControlMessage::Pong { client_ts_us, peer_ts_us: 0 }.encode_datagram()?).await?,
                            ControlMessage::Disconnect { .. } => return Ok(()),
                            message => { let _ = events.send(NetworkEvent::ControlReceived(message)); }
                        },
                        _ => {}
                    }
                }
            },
            Some(packet) = input.recv() => {
                let relative = motion::relative(&packet);
                let bytes = relative_motion.encode(packet);
                if relative {
                    // At the end of a mouse movement there may be no next sample
                    // to carry the history. Send one trailing repair copy.
                    motion_replay = Some((Instant::now() + Duration::from_millis(6), bytes.clone()));
                }
                link.send(bytes).await?;
            }
            _ = async {
                if let Some((deadline, _)) = &motion_replay {
                    tokio::time::sleep_until((*deadline).into()).await;
                }
            }, if motion_replay.is_some() => {
                if let Some((_, bytes)) = motion_replay.take() {
                    link.send(bytes).await?;
                }
            }
            Some(message) = controls.recv() => {
                if matches!(message, ControlMessage::RequestKeyframe { .. }) {
                    waiting = true;
                } else {
                    link.send(message.encode_datagram()?).await?;
                }
            }
            _ = tokio::time::sleep_until((last_pli + PLI_INTERVAL).into()), if waiting => {
                last_pli = Instant::now();
                link.send(ControlMessage::RequestKeyframe {
                    last_good_frame_id: None,
                    reason: "viewer lost a reference".into(),
                }.encode_datagram()?).await?;
            }
            _ = tick.tick() => {
                ensure!(last_seen.elapsed() < IDLE_TIMEOUT, "Host stopped responding");
                if last_video.elapsed() >= VIDEO_SILENCE {
                    waiting = true;
                }
                packets += link.take_video_packets();
                window_bytes += link.take_video_bytes();
                let path = link.path();
                link.send(ControlMessage::TelemetryFeedback {
                    rtt_ms: path.rtt.as_secs_f64() * 1000.0, loss_pct: path.loss,
                    frames_received: frames, frames_dropped: dropped, fec_recovered: recovered,
                    target_bitrate_bps: 0,
                }.encode_datagram()?).await?;
                let mut snapshot = telemetry.lock().await;
                report_path(&mut snapshot, Some(path));
                snapshot.video_packets_rx = packets;
                snapshot.video_frames_rx = frames;
                snapshot.video_frames_dropped = dropped;
                snapshot.fec_recovered_shards = recovered;
                snapshot.audio_packets_rx = audio;
                if window_start.elapsed() >= Duration::from_secs(1) {
                    snapshot.media_bitrate_mbps = (window_bytes as f64 * 8e-6 / window_start.elapsed().as_secs_f64()) as f32;
                    window_bytes = 0;
                    window_start = Instant::now();
                }
                let _ = events.send(NetworkEvent::TelemetryUpdated(Box::new(snapshot.clone())));
            }
        }
    }
}

fn report_path(snapshot: &mut NetworkTelemetry, path: Option<Path>) {
    snapshot.connection_path = path.map(|path| path.kind);
    snapshot.active_peer_addr = path.and_then(|path| path.address);
    snapshot.rtt_ms = path.map_or(0.0, |path| path.rtt.as_secs_f64() * 1000.0);
    snapshot.packet_loss_pct = path.map_or(0.0, |path| path.loss);
    snapshot.peer_packet_loss_pct = path.map(|path| path.loss);
    snapshot.relayed =
        path.is_some_and(|path| matches!(path.kind, ConnectionPath::Relay | ConnectionPath::Turn));
}

fn bitrate_decision(rate: u32, path: Option<Path>) -> BitrateDecision {
    BitrateDecision {
        target_bitrate_bps: rate,
        reason: "shared upload and path budget".into(),
        state: CongestionState::Stable,
        rtt_ms: path.map_or(0.0, |path| path.rtt.as_secs_f64() * 1000.0),
        min_rtt_ms: 0.0,
        loss_pct: path.map_or(0.0, |path| path.loss),
        encoder_reconfigure_recommended: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{
        SessionHandle, TransportProtocol, start_client_session, start_host_session,
    };

    fn config(relay_only: bool) -> NetworkConfig {
        NetworkConfig {
            protocol: TransportProtocol::Auto,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            enable_stun: false,
            relay_only,
            max_bitrate_bps: 24_000_000,
            upload_budget_bps: Some(24_000_000),
            ..Default::default()
        }
    }

    async fn invite(host: &mut SessionHandle) -> String {
        loop {
            if let NetworkEvent::InviteReady { invite_uri, .. } =
                host.event_rx.recv().await.unwrap()
            {
                return invite_uri;
            }
        }
    }

    async fn connected(viewer: &mut SessionHandle) {
        loop {
            match viewer.event_rx.recv().await.unwrap() {
                NetworkEvent::PhaseChanged(ConnectionPhase::Connected) => return,
                NetworkEvent::PhaseChanged(ConnectionPhase::Failed) => panic!(
                    "{}",
                    viewer.telemetry.lock().await.last_error.as_ref().unwrap()
                ),
                _ => {}
            }
        }
    }

    fn frame(id: u32) -> OutboundVideoFrame {
        OutboundVideoFrame {
            meta: FrameTelemetry::full_range_hardware(0, 1920, 1080),
            codec: VideoCodec::H264,
            frame_id: id,
            timestamp_us: u64::from(id) * 16_667,
            captured_at: Instant::now(),
            fps: 60,
            is_keyframe: true,
            payload: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88, 0x80, 0x00]),
        }
    }

    use crate::telemetry::FrameTelemetry;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires VMAX_TEST_REGIONAL_URL and VMAX_REGIONAL_HOST_TOKEN for a local regional server"]
    async fn regional_room_code_connects_over_raw_udp_and_webrtc() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        tokio::time::timeout(Duration::from_secs(35), async {
            let mut cfg = config(false);
            cfg.signaling_url = std::env::var("VMAX_TEST_REGIONAL_URL").unwrap();
            let mut host = start_host_session(cfg.clone()).await.unwrap();
            let full_invite = invite(&mut host).await;
            let code = host.telemetry.lock().await.room_code.clone().unwrap();
            assert!(regional::is_code(&code));
            assert_eq!(
                regional::lookup(&regional::endpoint(&cfg.signaling_url).unwrap(), &code)
                    .await
                    .unwrap(),
                full_invite,
            );
            let mut raw = start_client_session(cfg.clone(), code).await.unwrap();
            connected(&mut raw).await;
            assert!(matches!(
                raw.telemetry.lock().await.connection_path,
                Some(ConnectionPath::LanRawUdp)
            ));
            let rtc_cfg = NetworkConfig {
                bind_addr: "[::1]:0".parse().unwrap(),
                signaling_url: NetworkConfig::default().signaling_url,
                ..cfg
            };
            let mut rtc = start_client_session(rtc_cfg, full_invite).await.unwrap();
            connected(&mut rtc).await;
            assert!(matches!(
                rtc.telemetry.lock().await.connection_path,
                Some(ConnectionPath::DirectInternet | ConnectionPath::Relay | ConnectionPath::Turn)
            ));
            while host.telemetry.lock().await.active_players != 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            host.tx.video_tx.publish(frame(1)).unwrap();
            for viewer in [&mut raw, &mut rtc] {
                loop {
                    let event = viewer.event_rx.recv().await.unwrap();
                    if let NetworkEvent::VideoFrameReceived(unit) = event {
                        assert_eq!(unit.frame_id, 1);
                        break;
                    }
                    assert!(!matches!(
                        event,
                        NetworkEvent::PhaseChanged(ConnectionPhase::Failed)
                    ));
                }
            }
            host.tx.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn auto_room_fans_out_to_four_mixed_viewers_and_reuses_a_slot() {
        tokio::time::timeout(Duration::from_secs(55), async {
            let cfg = config(false);
            let mut host = start_host_session(cfg.clone()).await.unwrap();
            let code = invite(&mut host).await;
            let mut viewers = Vec::new();
            for index in 0..4 {
                let mut viewer_config = cfg.clone();
                if index == 3 {
                    viewer_config.bind_addr = "[::1]:0".parse().unwrap();
                }
                let mut viewer = start_client_session(viewer_config, code.clone())
                    .await
                    .unwrap();
                connected(&mut viewer).await;
                let snapshot = viewer.telemetry.lock().await;
                assert_eq!(snapshot.player_slot, Some(index));
                assert_eq!(
                    matches!(snapshot.connection_path, Some(ConnectionPath::LanRawUdp)),
                    index < 3
                );
                drop(snapshot);
                viewers.push(viewer);
            }
            while host.telemetry.lock().await.active_players != 4 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(host.telemetry.lock().await.target_bitrate_bps, 6_000_000);
            let extra = start_client_session(cfg.clone(), code.clone())
                .await
                .unwrap();
            assert_eq!(
                extra.join_handle.await.unwrap().unwrap_err().to_string(),
                "Host is full (4/4)"
            );
            host.tx.video_tx.publish(frame(1)).unwrap();
            for viewer in &mut viewers {
                loop {
                    if let NetworkEvent::VideoFrameReceived(unit) =
                        viewer.event_rx.recv().await.unwrap()
                    {
                        assert_eq!(unit.frame_id, 1);
                        assert_eq!(unit.meta.width, 1920);
                        assert!(unit.is_keyframe);
                        break;
                    }
                }
            }
            viewers[2]
                .tx
                .input_tx
                .send(InputPacket {
                    seq: 1,
                    client_ts_us: 0,
                    event: crate::protocol::InputEvent::GamepadState {
                        index: 0,
                        buttons: 1,
                        left_trigger: 0,
                        right_trigger: 0,
                        left_x: 0,
                        left_y: 0,
                        right_x: 0,
                        right_y: 0,
                    },
                })
                .await
                .unwrap();
            loop {
                if let NetworkEvent::PlayerInput { slot, packet } =
                    host.event_rx.recv().await.unwrap()
                {
                    assert_eq!(slot, 2);
                    assert_eq!(packet.seq, 1);
                    break;
                }
            }
            let viewer = viewers.remove(2);
            viewer.tx.close();
            let _ = viewer.join_handle.await.unwrap();
            loop {
                if let NetworkEvent::PlayerPresence(PlayerPresence {
                    slot: 2,
                    joined: false,
                }) = host.event_rx.recv().await.unwrap()
                {
                    break;
                }
            }
            let mut reconnect = start_client_session(cfg, code).await.unwrap();
            connected(&mut reconnect).await;
            assert_eq!(reconnect.telemetry.lock().await.player_slot, Some(2));
            host.tx.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn strict_mode_refuses_the_third_h264_viewer_on_a_thirty_megabit_uplink() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut cfg = config(false);
            cfg.strict_upload_bps = Some(24_000_000);
            cfg.host_codec = Some(VideoCodec::H264);
            let mut host = start_host_session(cfg.clone()).await.unwrap();
            let code = invite(&mut host).await;
            let mut first = start_client_session(cfg.clone(), code.clone())
                .await
                .unwrap();
            let mut second = start_client_session(cfg.clone(), code.clone())
                .await
                .unwrap();
            tokio::join!(connected(&mut first), connected(&mut second));
            let third = start_client_session(cfg, code).await.unwrap();
            assert_eq!(
                third.join_handle.await.unwrap().unwrap_err().to_string(),
                crate::network::BANDWIDTH_FULL
            );
            host.tx.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_room_falls_back_to_encrypted_webrtc_and_delivers_an_access_unit() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let cfg = config(false);
            let mut host = start_host_session(cfg.clone()).await.unwrap();
            let viewer_config = NetworkConfig {
                bind_addr: "[::1]:0".parse().unwrap(),
                ..cfg
            };
            let mut viewer = start_client_session(viewer_config, invite(&mut host).await)
                .await
                .unwrap();
            connected(&mut viewer).await;
            while host.telemetry.lock().await.active_players != 1 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let mut video = frame(1);
            let mut payload = video.payload.to_vec();
            payload.resize(24_000, 0x55);
            video.payload = Bytes::from(payload);
            let expected = video.payload.clone();
            host.tx.video_tx.publish(video).unwrap();
            loop {
                match viewer.event_rx.recv().await.unwrap() {
                    NetworkEvent::VideoFrameReceived(unit) => {
                        assert_eq!(unit.frame_id, 1);
                        assert_eq!(unit.meta.width, 1920);
                        assert_eq!(unit.payload, expected);
                        break;
                    }
                    NetworkEvent::PhaseChanged(ConnectionPhase::Failed) => {
                        panic!("{:?}", viewer.telemetry.lock().await.last_error)
                    }
                    _ => {}
                }
            }
            host.tx.close();
        })
        .await
        .unwrap();
    }
}
