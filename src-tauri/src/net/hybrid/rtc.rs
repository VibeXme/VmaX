use anyhow::{Context, Result, bail, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::endpoint::Connection;
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
    bwe::{Bitrate, BweKind},
    change::{SdpAnswer, SdpOffer},
    channel::{ChannelConfig, ChannelId as DataChannel, Reliability},
    format::Codec,
    media::{Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
    rtp::{Extension, ExtensionSerializer, ExtensionValues},
};
use tokio::net::UdpSocket;

use super::{Incoming, Path, StreamTransport, setup};
use crate::{
    network::{ConnectionPath, NetworkConfig, OutboundVideoFrame},
    protocol::{
        AudioCodec, AudioPacket, AudioPacketHeader, ChannelId, ControlMessage, InputPacket,
        ReassembledFrame, VideoCodec,
    },
    stun::{canonical_addr, gather_candidates, socket_target},
    telemetry::{FrameTelemetry, TELEMETRY_LEN},
    turn_relay::TurnRelay,
};

const CONTROL: &str = "vmax-control";
const MOTION: &str = "vmax-motion";
const EXT_ID: u8 = 9;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(9);

pub(super) struct RtcLink {
    rtc: Rtc,
    udp: UdpSocket,
    local: Vec<SocketAddr>,
    turn: Option<TurnRelay>,
    remote_turn: Option<SocketAddr>,
    carrier: Connection,
    virtual_local: SocketAddr,
    virtual_remote: SocketAddr,
    path: Path,
    video: Option<Mid>,
    audio: Option<Mid>,
    control: Option<DataChannel>,
    motion: Option<DataChannel>,
    queue: VecDeque<Incoming>,
    pending_frame: Option<ReassembledFrame>,
    received: u64,
    sent: u64,
    receive_base: u64,
    send_base: u64,
    configured_rate: u32,
    connected: bool,
    video_needs_sync: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Description<T> {
    sdp: T,
    turn: Option<SocketAddr>,
}

impl RtcLink {
    pub(super) async fn connect(
        udp: UdpSocket,
        carrier: Connection,
        mut signaling: setup::Exchange,
        config: &NetworkConfig,
        host: bool,
        relay_only: bool,
    ) -> Result<Self> {
        let grant = signaling.turn().cloned();
        let regional_stun = grant
            .as_ref()
            .map(|grant| vec![format!("{}:{}", grant.domain, grant.udp_port)]);
        let stun = regional_stun.as_deref().unwrap_or(&config.stun_servers);
        let (candidates, turn) = tokio::join!(
            gather_candidates(&udp, stun, config.enable_stun && !relay_only),
            async {
                if let Some(grant) = grant
                    && grant.expires_at
                        > std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                    && let Some(turn) = TurnRelay::regional(
                        &grant.domain,
                        grant.udp_port,
                        grant.tls_port,
                        grant.username,
                        grant.password,
                    )
                    .await
                {
                    return Some(turn);
                }
                TurnRelay::configured().await
            },
        );
        let own_turn = turn.as_ref().and_then(TurnRelay::local_addr);
        let virtual_local = virtual_addr(host);
        let virtual_remote = virtual_addr(!host);
        let rtc = Rtc::builder()
            .clear_codecs()
            .enable_h264(true)
            .enable_h265(true)
            .enable_opus(true, false)
            .set_extension(
                EXT_ID,
                Extension::with_serializer("urn:vmax:frame-meta:1", MetaExtension),
            )
            .set_reordering_size_video(32)
            .set_stats_interval(Some(Duration::from_millis(500)))
            .enable_bwe(Some(Bitrate::bps(u64::from(config.initial_bitrate_bps))))
            .build(Instant::now());
        let mut link = Self {
            rtc,
            udp,
            local: candidates.lan_addrs.clone(),
            turn,
            remote_turn: None,
            carrier,
            virtual_local,
            virtual_remote,
            path: Path {
                kind: ConnectionPath::DirectInternet,
                address: None,
                rtt: Duration::ZERO,
                loss: 0.0,
                estimate: None,
            },
            video: None,
            audio: None,
            control: None,
            motion: None,
            queue: VecDeque::new(),
            pending_frame: None,
            received: 0,
            sent: 0,
            receive_base: 0,
            send_base: 0,
            configured_rate: 0,
            connected: false,
            video_needs_sync: true,
        };
        link.drain().await?;
        if !relay_only {
            for addr in &candidates.lan_addrs {
                if let Ok(candidate) = Candidate::host(*addr, "udp") {
                    link.rtc.add_local_candidate(candidate);
                    link.drain().await?;
                }
            }
            for addr in [candidates.public_reflexive, candidates.public_reflexive_v6]
                .into_iter()
                .flatten()
            {
                if let Some(base) = link
                    .local
                    .iter()
                    .copied()
                    .find(|base| base.is_ipv6() == addr.is_ipv6())
                    && let Ok(candidate) = Candidate::server_reflexive(addr, base, "udp")
                {
                    link.rtc.add_local_candidate(candidate);
                    link.drain().await?;
                }
            }
        }
        if let Some(addr) = own_turn {
            link.rtc
                .add_local_candidate(Candidate::relayed(addr, addr, "udp")?);
            link.drain().await?;
        }
        // Keep a relay route available even for installations without TURN
        // credentials. The browser-independent RTC stack still authenticates
        // and encrypts its SRTP and SCTP traffic end to end over this carrier.
        link.rtc
            .add_local_candidate(Candidate::relayed(virtual_local, virtual_local, "udp")?);
        link.drain().await?;
        if host {
            let mut change = link.rtc.sdp_api();
            link.video =
                Some(change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None));
            link.audio =
                Some(change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None));
            change.add_channel(CONTROL.into());
            change.add_channel_with_config(ChannelConfig {
                label: MOTION.into(),
                ordered: false,
                reliability: Reliability::MaxRetransmits { retransmits: 0 },
                ..Default::default()
            });
            let (sdp, pending) = change.apply().context("Could not create a WebRTC offer")?;
            link.drain().await?;
            signaling
                .send(&Description {
                    sdp,
                    turn: own_turn,
                })
                .await?;
            let answer: Description<SdpAnswer> = signaling.recv().await?;
            link.remote_turn = answer.turn;
            link.rtc.sdp_api().accept_answer(pending, answer.sdp)?;
            link.drain().await?;
        } else {
            let offer: Description<SdpOffer> = signaling.recv().await?;
            link.remote_turn = offer.turn;
            let sdp = link.rtc.sdp_api().accept_offer(offer.sdp)?;
            link.drain().await?;
            signaling
                .send(&Description {
                    sdp,
                    turn: own_turn,
                })
                .await?;
        }
        signaling.finish().await?;
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            while !link.connected || link.control.is_none() || link.motion.is_none() {
                link.pump().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("WebRTC ICE or DTLS handshake timed out")??;
        Ok(link)
    }

    async fn drain(&mut self) -> Result<Instant> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(deadline) => return Ok(deadline),
                Output::Event(event) => self.event(event)?,
                Output::Transmit(packet) => {
                    let source = canonical_addr(packet.source);
                    let target = canonical_addr(packet.destination);
                    if source == self.virtual_local {
                        if target == self.virtual_remote {
                            let _ = self
                                .carrier
                                .send_datagram(Bytes::copy_from_slice(&packet.contents));
                        }
                    } else if target != self.virtual_remote {
                        if let Some(allocation) = self
                            .turn
                            .as_ref()
                            .filter(|turn| turn.local_addr() == Some(source))
                        {
                            if crate::turn_relay::is_public_peer(target) {
                                let _ = allocation.conn().send_to(&packet.contents, target).await;
                            }
                        } else {
                            let _ = self.udp.try_send_to(
                                &packet.contents,
                                socket_target(target, self.udp.local_addr()?.is_ipv6()),
                            );
                        }
                    }
                }
            }
        }
    }

    async fn pump(&mut self) -> Result<()> {
        let deadline = self.drain().await?;
        let mut udp = [0; 2048];
        let mut relay = [0; 2048];
        let allocation = self.turn.as_ref().map(TurnRelay::conn);
        let relay_addr = self.turn.as_ref().and_then(TurnRelay::local_addr);
        let tick = tokio::time::sleep_until(deadline.into());
        let input = tokio::select! {
            _ = tick => Input::Timeout(Instant::now()),
            packet = self.carrier.read_datagram() => {
                let packet = packet?;
                self.handle_datagram(self.virtual_remote, self.virtual_local, &packet).await?;
                return Ok(());
            }
            received = self.udp.recv_from(&mut udp) => {
                let Ok((len, source)) = received else { return Ok(()) };
                let source = canonical_addr(source);
                let destination = self.local.iter().copied().find(|addr| {
                    source.is_ipv6() == addr.is_ipv6() && source.ip().is_loopback() == addr.ip().is_loopback()
                }).or_else(|| self.local.iter().copied().find(|addr| source.is_ipv6() == addr.is_ipv6()));
                if let Some(destination) = destination {
                    self.handle_datagram(source, destination, &udp[..len]).await?;
                }
                return Ok(());
            }
            received = async {
                if let Some(conn) = allocation { conn.recv_from(&mut relay).await } else { std::future::pending().await }
            } => {
                if let (Ok((len, source)), Some(destination)) = (received, relay_addr) {
                    self.handle_datagram(source, destination, &relay[..len]).await?;
                }
                return Ok(());
            }
        };
        self.rtc.handle_input(input)?;
        self.drain().await?;
        Ok(())
    }

    async fn handle_datagram(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
    ) -> Result<()> {
        if let Ok(receive) = Receive::new(Protocol::Udp, source, destination, bytes) {
            let input = Input::Receive(Instant::now(), receive);
            if self.rtc.accepts(&input) {
                self.rtc.handle_input(input)?;
                self.drain().await?;
            }
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Connected => self.connected = true,
            Event::Closed => bail!("WebRTC peer disconnected"),
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) if self.connected => {
                bail!("WebRTC ICE disconnected")
            }
            Event::MediaAdded(media) => match media.kind {
                MediaKind::Video => self.video = Some(media.mid),
                MediaKind::Audio => self.audio = Some(media.mid),
            },
            Event::ChannelOpen(id, label) if label == CONTROL => self.control = Some(id),
            Event::ChannelOpen(id, label) if label == MOTION => self.motion = Some(id),
            Event::ChannelData(data) if data.binary && data.data.len() <= 8192 => {
                if self.queue.len() >= 64 {
                    bail!("WebRTC input queue exceeded");
                }
                self.queue
                    .push_back(Incoming::Packet(Bytes::from(data.data)));
            }
            Event::PeerStats(stats) => {
                if let Some(pair) = stats.selected_candidate_pair {
                    let local = pair.local.addr;
                    let remote = pair.remote.addr;
                    self.path.kind = if local == self.virtual_local || remote == self.virtual_remote
                    {
                        if self
                            .carrier
                            .paths()
                            .iter()
                            .any(|path| path.is_selected() && path.is_ip())
                        {
                            ConnectionPath::DirectInternet
                        } else {
                            ConnectionPath::Relay
                        }
                    } else if self.turn.as_ref().and_then(TurnRelay::local_addr) == Some(local)
                        || self.remote_turn == Some(remote)
                    {
                        ConnectionPath::Turn
                    } else {
                        ConnectionPath::DirectInternet
                    };
                    self.path.address = (remote != self.virtual_remote).then_some(remote);
                    self.path.rtt = pair
                        .current_round_trip_time
                        .or(stats.rtt)
                        .unwrap_or(self.path.rtt);
                }
                self.path.loss = 100.0
                    * f64::from(
                        stats
                            .egress_loss_fraction
                            .or(stats.ingress_loss_fraction)
                            .unwrap_or(0.0),
                    );
            }
            Event::MediaIngressStats(stats) if Some(stats.mid) == self.video => {
                self.received += stats.packets.saturating_sub(self.receive_base);
                self.receive_base = stats.packets;
            }
            Event::MediaEgressStats(stats) if Some(stats.mid) == self.video => {
                self.sent += stats.packets.saturating_sub(self.send_base);
                self.send_base = stats.packets;
            }
            Event::EgressBitrateEstimate(BweKind::Twcc {
                estimate,
                can_probe: true,
            })
            | Event::EgressBitrateEstimate(BweKind::Remb { estimate, .. }) => {
                self.path.estimate = Some(estimate.as_f64().clamp(1.0, u32::MAX as f64) as u32);
            }
            Event::KeyframeRequest(_) => self.queue.push_back(Incoming::Keyframe),
            Event::MediaData(data) => {
                let Some(meta) = data.ext_vals.user_values.get::<RtpMeta>() else {
                    return Ok(());
                };
                match *meta {
                    RtpMeta::Video {
                        frame_id,
                        timestamp_us,
                        codec,
                        meta,
                        is_keyframe,
                    } => {
                        if !data.contiguous && !is_keyframe {
                            self.queue.push_back(Incoming::Keyframe);
                            return Ok(());
                        }
                        let frame = ReassembledFrame {
                            frame_id,
                            codec,
                            timestamp_us,
                            meta,
                            is_keyframe,
                            fec_recovered_shards: 0,
                            payload: Bytes::from_owner(data.data),
                        };
                        if self.pending_frame.replace(frame).is_some() {
                            self.queue.push_back(Incoming::Keyframe);
                        }
                    }
                    RtpMeta::Audio(header) => {
                        if self.queue.len() < 64 {
                            self.queue.push_back(Incoming::Packet(
                                AudioPacket {
                                    header,
                                    payload: Bytes::from_owner(data.data),
                                }
                                .encode(),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn media(
        &mut self,
        mid: Mid,
        codec: Codec,
        stamp: u64,
        bytes: Bytes,
        meta: RtpMeta,
    ) -> Result<()> {
        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(());
        };
        let pt = writer
            .payload_params()
            .find(|params| params.spec().codec == codec)
            .map(|params| params.pt());
        let Some(pt) = pt else {
            bail!("Codec was not negotiated for WebRTC");
        };
        writer.user_extension_value(meta).write(
            pt,
            Instant::now(),
            MediaTime::from_micros(stamp),
            Arc::<[u8]>::from(bytes.as_ref()),
        )?;
        self.drain().await?;
        Ok(())
    }
}

impl StreamTransport for RtcLink {
    fn path(&self) -> Path {
        self.path
    }

    async fn video(&mut self, frame: OutboundVideoFrame, rate: u32) -> Result<()> {
        let Some(mid) = self.video else { return Ok(()) };
        let codec = match frame.codec {
            VideoCodec::H264 => Codec::H264,
            VideoCodec::Hevc => Codec::H265,
            _ => return Ok(()),
        };
        if !frame.is_keyframe && self.video_needs_sync {
            return Ok(());
        }
        let queued = self
            .rtc
            .direct_api()
            .stream_tx_by_mid(mid, None)
            .and_then(|stream| stream.queue_info())
            .and_then(|info| info.first_unsent())
            .is_some_and(|first| first.elapsed() > Duration::from_millis(18));
        self.drain().await?;
        if queued || (!frame.is_keyframe && frame.captured_at.elapsed() > Duration::from_millis(35))
        {
            self.video_needs_sync = true;
            if !self
                .queue
                .iter()
                .any(|event| matches!(event, Incoming::Keyframe))
            {
                self.queue.push_back(Incoming::Keyframe);
            }
            return Ok(());
        }
        if self.configured_rate.abs_diff(rate) >= 100_000 {
            self.rtc
                .bwe()
                .set_bitrate(Bitrate::bps(0), Bitrate::bps(u64::from(rate)));
            self.drain().await?;
            self.configured_rate = rate;
        }
        self.media(
            mid,
            codec,
            frame.timestamp_us,
            frame.payload,
            RtpMeta::Video {
                frame_id: frame.frame_id,
                timestamp_us: frame.timestamp_us,
                codec: frame.codec as u8,
                meta: frame.meta,
                is_keyframe: frame.is_keyframe,
            },
        )
        .await?;
        self.video_needs_sync = false;
        Ok(())
    }

    async fn send(&mut self, packet: Bytes) -> Result<()> {
        if matches!(
            ControlMessage::decode_datagram(&packet),
            Ok(ControlMessage::RequestKeyframe { .. })
        ) && let Some(mid) = self.video
            && let Some(mut writer) = self.rtc.writer(mid)
        {
            let requested = writer
                .request_keyframe(None, KeyframeRequestKind::Pli)
                .is_ok();
            self.drain().await?;
            if requested {
                return Ok(());
            }
        }
        if packet.first() == Some(&(ChannelId::Audio as u8))
            && let Some(mid) = self.audio
            && let Ok(audio) = AudioPacket::decode(&packet)
            && audio.header.codec == AudioCodec::Opus as u8
        {
            return self
                .media(
                    mid,
                    Codec::Opus,
                    audio.header.timestamp_us,
                    audio.payload,
                    RtpMeta::Audio(audio.header),
                )
                .await;
        }
        let fast = match packet
            .first()
            .and_then(|tag| ChannelId::try_from(*tag).ok())
        {
            Some(ChannelId::Input) => {
                InputPacket::decode(&packet).is_ok_and(|input| input.is_high_frequency_motion())
            }
            Some(ChannelId::Control) => false,
            _ => true,
        };
        let id = if fast { self.motion } else { self.control };
        if let Some(id) = id
            && let Some(mut channel) = self.rtc.channel(id)
        {
            let sent = channel.write(true, &packet)?;
            self.drain().await?;
            ensure!(
                sent || fast,
                "The WebRTC control channel exceeded its send limit"
            );
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Incoming> {
        loop {
            if let Some(frame) = self.pending_frame.take() {
                return Ok(Incoming::Video(Box::new(frame)));
            }
            if let Some(message) = self.queue.pop_front() {
                return Ok(message);
            }
            self.pump().await?;
        }
    }

    fn take_video_packets(&mut self) -> u64 {
        std::mem::take(&mut self.received)
    }
    fn take_sent_packets(&mut self) -> u64 {
        std::mem::take(&mut self.sent)
    }
}

fn virtual_addr(host: bool) -> SocketAddr {
    ([192, 0, 2, if host { 1 } else { 2 }], 45000).into()
}

#[derive(Clone, Copy)]
enum RtpMeta {
    Video {
        frame_id: u32,
        timestamp_us: u64,
        codec: u8,
        meta: FrameTelemetry,
        is_keyframe: bool,
    },
    Audio(AudioPacketHeader),
}

#[derive(Debug)]
struct MetaExtension;

impl ExtensionSerializer for MetaExtension {
    fn write_to(&self, out: &mut [u8], value: &ExtensionValues) -> usize {
        let Some(meta) = value.user_values.get::<RtpMeta>() else {
            return 0;
        };
        let mut bytes = BytesMut::with_capacity(32);
        match *meta {
            RtpMeta::Video {
                frame_id,
                timestamp_us,
                codec,
                meta,
                is_keyframe,
            } => {
                bytes.put_u8(1);
                bytes.put_u32(frame_id);
                bytes.put_u64(timestamp_us);
                bytes.put_u8(codec);
                bytes.put_u8(u8::from(is_keyframe));
                meta.encode(&mut bytes);
            }
            RtpMeta::Audio(header) => {
                bytes.put_u8(2);
                bytes.put_u32(header.seq);
                bytes.put_u64(header.timestamp_us);
                bytes.extend_from_slice(&[header.codec, header.flags, header.channels]);
            }
        }
        if out.len() < bytes.len() {
            return 0;
        }
        out[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    }

    fn parse_value(&self, bytes: &[u8], value: &mut ExtensionValues) -> bool {
        let meta = match bytes {
            [1, rest @ ..] if rest.len() == 14 + TELEMETRY_LEN => {
                let Some(meta) = FrameTelemetry::decode(&rest[14..]) else {
                    return false;
                };
                RtpMeta::Video {
                    frame_id: u32::from_be_bytes(rest[..4].try_into().unwrap()),
                    timestamp_us: u64::from_be_bytes(rest[4..12].try_into().unwrap()),
                    codec: rest[12],
                    is_keyframe: rest[13] != 0,
                    meta,
                }
            }
            [2, rest @ ..] if rest.len() == 15 => RtpMeta::Audio(AudioPacketHeader {
                seq: u32::from_be_bytes(rest[..4].try_into().unwrap()),
                timestamp_us: u64::from_be_bytes(rest[4..12].try_into().unwrap()),
                codec: rest[12],
                flags: rest[13],
                channels: rest[14],
            }),
            _ => return false,
        };
        value.user_values.set(meta);
        true
    }

    fn is_video(&self) -> bool {
        true
    }
    fn is_audio(&self) -> bool {
        true
    }
    fn requires_two_byte_form(&self, _: &ExtensionValues) -> bool {
        true
    }
}
