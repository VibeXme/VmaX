use anyhow::{Result, bail, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use reed_solomon_erasure::galois_8::ReedSolomon;
use ring::aead;
use rusty_enet::{EventNoRef, Host, HostSettings, MTU_MAX, Packet, PacketReceived, PeerID, Socket};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::SocketAddr,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

use super::{Incoming, Path, StreamTransport};
use crate::{
    network::{ConnectionPath, OutboundVideoFrame},
    protocol::{ChannelId, InputPacket, ReassembledFrame},
    stun::socket_target,
    telemetry::{FrameTelemetry, TELEMETRY_LEN},
};

const ENET: u8 = 0xfe;
const HEADER: usize = 12;
const BODY: usize = 1160 - HEADER - 24; // ciphertext and sequence number fit in one datagram
const FRAME_INFO: usize = 1 + 8 + 4 + TELEMETRY_LEN;
const GROUP: usize = 16;
const MAX_SHARDS: usize = 4096;
const KEYFRAME_AGE: Duration = Duration::from_secs(5);
const KEYFRAME_PACING_MARGIN: Duration = Duration::from_millis(150);
const DELTA_AGE: Duration = Duration::from_millis(35);
const TICK: Duration = Duration::from_millis(3);

pub(super) struct RawLink {
    socket: UdpSocket,
    peer_addr: SocketAddr,
    enet: Host<EnetSocket>,
    peer: Option<PeerID>,
    cipher: Cipher,
    input: VecDeque<Incoming>,
    video: Option<Pending>,
    assembly: BTreeMap<u32, Assembly>,
    packets: u64,
    received_video_bytes: u64,
    sent_packets: u64,
    traffic_class: Option<u32>,
    last_completed: Option<u32>,
    loss: f64,
}

struct Pending {
    shards: std::vec::IntoIter<Bytes>,
    next: Instant,
    deadline: Instant,
    spacing: Duration,
    keyframe: bool,
}

impl RawLink {
    pub(super) async fn new(
        socket: UdpSocket,
        peer_addr: SocketAddr,
        send_key: [u8; 32],
        recv_key: [u8; 32],
        host: bool,
    ) -> Result<Self> {
        socket
            .connect(socket_target(peer_addr, socket.local_addr()?.is_ipv6()))
            .await?;
        let mut enet = Host::new(
            EnetSocket::default(),
            HostSettings {
                peer_limit: 1,
                channel_limit: 2,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("ENet setup: {e:?}"))?;
        enet.set_mtu(1136)
            .map_err(|e| anyhow::anyhow!("Invalid ENet datagram MTU: {e:?}"))?;
        if !host {
            enet.connect(peer_addr, 2, 0)
                .map_err(|e| anyhow::anyhow!("ENet connect: {e:?}"))?;
        }
        let mut link = Self {
            socket,
            peer_addr,
            enet,
            peer: None,
            cipher: Cipher::new(send_key, recv_key)?,
            input: VecDeque::new(),
            video: None,
            assembly: BTreeMap::new(),
            packets: 0,
            received_video_bytes: 0,
            sent_packets: 0,
            traffic_class: None,
            last_completed: None,
            loss: 0.0,
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while link.peer.is_none() {
                link.pump().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(link)
    }

    async fn packet(&mut self, bytes: &[u8]) -> Result<()> {
        let sealed = self.seal_packet(bytes)?;
        match self.socket.send(&sealed).await {
            Ok(_) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn seal_packet(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        use crate::wan_transport::{DSCP_AF41, DSCP_EF, mark_traffic_class};
        let class = if bytes.first() == Some(&(ChannelId::Video as u8)) {
            DSCP_AF41
        } else {
            DSCP_EF
        };
        if self.traffic_class != Some(class) {
            mark_traffic_class(&socket2::SockRef::from(&self.socket), class);
            self.traffic_class = Some(class);
        }
        self.cipher.seal(bytes)
    }

    fn send_due_video(&mut self) -> Result<()> {
        for _ in 0..8 {
            let now = Instant::now();
            let Some(video) = self.video.as_ref().filter(|video| video.next <= now) else {
                break;
            };
            if now >= video.deadline {
                self.video = None;
                self.input.push_back(Incoming::Keyframe);
                break;
            }
            let shard = video.shards.as_slice()[0].clone();
            let sealed = self.seal_packet(&shard)?;
            let video = self.video.as_mut().unwrap();
            match self.socket.try_send(&sealed) {
                Ok(_) => self.sent_packets += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    video.next = now + Duration::from_millis(1);
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            video.shards.next();
            video.next += video.spacing;
            if video.shards.len() == 0 {
                self.video = None;
                break;
            }
        }
        Ok(())
    }

    async fn flush_enet(&mut self) -> Result<()> {
        self.enet.flush();
        while let Some(data) = self.enet.socket_mut().outgoing.pop_front() {
            let mut packet = BytesMut::with_capacity(data.len() + 1);
            packet.put_u8(ENET);
            packet.extend_from_slice(&data);
            self.packet(&packet).await?;
        }
        Ok(())
    }

    async fn pump(&mut self) -> Result<()> {
        self.send_due_video()?;
        for _ in 0..32 {
            match self.enet.service()?.map(|event| event.no_ref()) {
                Some(EventNoRef::Connect { peer, .. }) => self.peer = Some(peer),
                Some(EventNoRef::Disconnect { .. }) => bail!("ENet peer disconnected"),
                Some(EventNoRef::Receive { packet, .. }) if self.input.len() < 32 => {
                    self.input
                        .push_back(Incoming::Packet(Bytes::copy_from_slice(packet.data())));
                }
                Some(_) => bail!("ENet input queue exceeded"),
                None => break,
            }
        }
        self.flush_enet().await?;
        if !self.input.is_empty() {
            return Ok(());
        }

        let mut buffer = [0; 1500];
        let next = self.video.as_ref().map_or(Instant::now() + TICK, |video| {
            video.next.min(Instant::now() + TICK)
        });
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(next.into()) => {
                self.send_due_video()?;
                let expired: Vec<_> = self.assembly.iter().filter(|(_, item)| item.started.elapsed() >= item.age())
                    .map(|(&id, _)| id).collect();
                for id in expired {
                    if let Some(item) = self.assembly.remove(&id) {
                        let sample = 100.0 * item.missing() as f64 / item.data as f64;
                        self.loss = self.loss * 0.8 + sample * 0.2;
                        self.input.push_back(Incoming::Keyframe);
                    }
                }
            }
            result = self.socket.recv(&mut buffer) => {
                let len = match result {
                    Ok(len) => len,
                    Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset) => return Ok(()),
                    Err(error) => return Err(error.into()),
                };
                let Some(bytes) = self.cipher.open(&mut buffer[..len]) else { return Ok(()) };
                match bytes.first().copied() {
                    Some(ENET) if bytes.len() <= MTU_MAX + 1 => {
                        self.enet.socket_mut().incoming.push_back((self.peer_addr, Bytes::copy_from_slice(&bytes[1..])));
                    }
                    Some(tag) if tag == ChannelId::Video as u8 => {
                        self.packets += 1;
                        self.received_video_bytes += bytes.len() as u64;
                        if let Some(frame) = self.collect(bytes)? {
                            self.input.push_back(Incoming::Video(Box::new(frame)));
                        }
                    }
                    Some(tag) if matches!(ChannelId::try_from(tag), Ok(ChannelId::Audio | ChannelId::Haptics)) => {
                        self.input.push_back(Incoming::Packet(Bytes::copy_from_slice(bytes)));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn collect(&mut self, bytes: &[u8]) -> Result<Option<ReassembledFrame>> {
        if bytes.len() != HEADER + BODY {
            return Ok(None);
        }
        let keyframe = bytes[1] == 1;
        let frame = u32::from_be_bytes(bytes[2..6].try_into()?);
        if self
            .last_completed
            .is_some_and(|last| (frame.wrapping_sub(last) as i32) <= 0)
        {
            return Ok(None);
        }
        let group = usize::from(u16::from_be_bytes(bytes[6..8].try_into()?));
        let shard = usize::from(u16::from_be_bytes(bytes[8..10].try_into()?));
        let data = usize::from(u16::from_be_bytes(bytes[10..12].try_into()?));
        if !(1..=MAX_SHARDS).contains(&data) || group >= data.div_ceil(GROUP) {
            return Ok(None);
        }
        let count = (data - group * GROUP).min(GROUP);
        if shard >= count + parity(count) {
            return Ok(None);
        }
        if self.assembly.len() >= 2 && !self.assembly.contains_key(&frame) {
            self.assembly.pop_first();
            self.input.push_back(Incoming::Keyframe);
        }
        let buffer = self
            .assembly
            .entry(frame)
            .or_insert_with(|| Assembly::new(data, keyframe));
        if buffer.data != data || buffer.keyframe != keyframe {
            return Ok(None);
        }
        buffer.put(group, count, shard, &bytes[HEADER..])?;
        if buffer.groups_remaining != 0 {
            return Ok(None);
        }
        let finished = self.assembly.remove(&frame).unwrap();
        self.assembly
            .retain(|&id, _| (id.wrapping_sub(frame) as i32) > 0);
        self.last_completed = Some(frame);
        self.loss = self.loss * 0.8 + (100.0 * finished.recovered as f64 / data as f64) * 0.2;
        finished.frame(frame)
    }
}

impl StreamTransport for RawLink {
    fn supports_main10(&self) -> bool {
        true
    }

    fn keyframe_in_flight(&self) -> bool {
        self.video.as_ref().is_some_and(|frame| frame.keyframe)
    }

    fn path(&self) -> Path {
        let rtt = self
            .peer
            .map(|peer| self.enet.peer(peer).round_trip_time())
            .unwrap_or_default();
        let lan = match self.peer_addr.ip() {
            std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
            std::net::IpAddr::V6(ip) => {
                ip.is_unique_local() || ip.is_loopback() || ip.is_unicast_link_local()
            }
        };
        Path {
            kind: if lan {
                ConnectionPath::LanRawUdp
            } else {
                ConnectionPath::DirectRawUdp
            },
            address: Some(self.peer_addr),
            rtt,
            loss: self.loss,
            estimate: None,
        }
    }

    async fn video(&mut self, frame: OutboundVideoFrame, rate: u32) -> Result<bool> {
        self.send_due_video()?;
        if self.video.is_some() || frame.payload.len() + FRAME_INFO > BODY * MAX_SHARDS {
            return Ok(false);
        }
        let wire_rate = crate::transport::paced_wire_rate(rate, 125).max(1);
        let spacing = Duration::from_secs_f64(1160.0 * 8.0 / f64::from(wire_rate));
        let keyframe = frame.is_keyframe;
        let shards = shards(frame)?;
        let age = if keyframe {
            // Include every FEC shard in the time budget while retaining the
            // configured wire rate. Reject frames that cannot finish in time.
            let needed = spacing * shards.len() as u32 + KEYFRAME_PACING_MARGIN;
            if needed > KEYFRAME_AGE {
                return Ok(false);
            }
            needed
        } else {
            DELTA_AGE
        };
        let now = Instant::now();
        // Charge only time in this one-frame send slot to the network budget.
        // Slow encode has already happened when the latest access unit arrives.
        let deadline = now + age;
        self.video = Some(Pending {
            shards: shards.into_iter(),
            next: now,
            deadline,
            spacing,
            keyframe,
        });
        Ok(true)
    }

    async fn send(&mut self, packet: Bytes) -> Result<()> {
        let Some(tag) = packet
            .first()
            .and_then(|byte| ChannelId::try_from(*byte).ok())
        else {
            return Ok(());
        };
        if matches!(tag, ChannelId::Input | ChannelId::Control) {
            let peer = self.peer.context("ENet is not connected")?;
            let fast = tag == ChannelId::Input
                && InputPacket::decode(&packet).is_ok_and(|input| input.is_high_frequency_motion());
            let control = if fast {
                Packet::unreliable(packet.as_ref())
            } else {
                Packet::reliable(packet.as_ref())
            };
            self.enet
                .peer_mut(peer)
                .send(u8::from(fast), &control)
                .map_err(|e| anyhow::anyhow!("ENet send: {e:?}"))?;
            self.flush_enet().await
        } else {
            self.packet(&packet).await
        }
    }

    async fn next(&mut self) -> Result<Incoming> {
        loop {
            if let Some(message) = self.input.pop_front() {
                return Ok(message);
            }
            self.pump().await?;
        }
    }

    fn take_video_bytes(&mut self) -> u64 {
        std::mem::take(&mut self.received_video_bytes)
    }

    fn take_video_packets(&mut self) -> u64 {
        std::mem::take(&mut self.packets)
    }
    fn take_sent_packets(&mut self) -> u64 {
        std::mem::take(&mut self.sent_packets)
    }
}

use anyhow::Context;

#[derive(Default)]
struct EnetSocket {
    incoming: VecDeque<(SocketAddr, Bytes)>,
    outgoing: VecDeque<Bytes>,
}

impl Socket for EnetSocket {
    type Address = SocketAddr;
    type Error = io::Error;

    fn send(&mut self, _: SocketAddr, buffer: &[u8]) -> io::Result<usize> {
        if self.outgoing.len() < 64 {
            self.outgoing.push_back(Bytes::copy_from_slice(buffer));
        }
        Ok(buffer.len())
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; MTU_MAX],
    ) -> io::Result<Option<(SocketAddr, PacketReceived)>> {
        Ok(self.incoming.pop_front().map(|(address, data)| {
            buffer[..data.len()].copy_from_slice(&data);
            (address, PacketReceived::Complete(data.len()))
        }))
    }
}

struct Cipher {
    send: aead::LessSafeKey,
    recv: aead::LessSafeKey,
    next: u64,
    highest: u64,
    // The replay window tolerates packet reordering across a large IDR.
    replay: [Option<u64>; 1024],
}

impl Cipher {
    fn new(send: [u8; 32], recv: [u8; 32]) -> Result<Self> {
        ensure!(send != recv, "Invalid UDP encryption keys");
        let key = |bytes| {
            aead::UnboundKey::new(&aead::CHACHA20_POLY1305, bytes)
                .map(aead::LessSafeKey::new)
                .map_err(|_| anyhow::anyhow!("UDP encryption initialization failed"))
        };
        Ok(Self {
            send: key(&send)?,
            recv: key(&recv)?,
            next: 0,
            highest: 0,
            replay: [None; 1024],
        })
    }

    fn seal(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let seq = self.next;
        self.next = seq.checked_add(1).context("UDP sequence exhausted")?;
        let mut out = Vec::with_capacity(bytes.len() + 24);
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(bytes);
        let tag = self
            .send
            .seal_in_place_separate_tag(nonce(seq), aead::Aad::empty(), &mut out[8..])
            .map_err(|_| anyhow::anyhow!("UDP encryption failed"))?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }

    fn open<'a>(&mut self, bytes: &'a mut [u8]) -> Option<&'a [u8]> {
        let seq = u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
        let slot = seq as usize % self.replay.len();
        if self.highest.saturating_sub(seq) >= self.replay.len() as u64
            || self.replay[slot] == Some(seq)
        {
            return None;
        }
        let plain = self
            .recv
            .open_in_place(nonce(seq), aead::Aad::empty(), &mut bytes[8..])
            .ok()?;
        self.highest = self.highest.max(seq);
        self.replay[slot] = Some(seq);
        Some(plain)
    }
}

fn nonce(sequence: u64) -> aead::Nonce {
    let mut bytes = [0; 12];
    bytes[4..].copy_from_slice(&sequence.to_be_bytes());
    aead::Nonce::assume_unique_for_key(bytes)
}

fn parity(data: usize) -> usize {
    data.div_ceil(8)
}

fn coder(data: usize) -> &'static ReedSolomon {
    static CODECS: OnceLock<[ReedSolomon; GROUP]> = OnceLock::new();
    &CODECS.get_or_init(|| {
        std::array::from_fn(|index| ReedSolomon::new(index + 1, parity(index + 1)).unwrap())
    })[data - 1]
}

fn shards(frame: OutboundVideoFrame) -> Result<Vec<Bytes>> {
    let data = (frame.payload.len() + FRAME_INFO).div_ceil(BODY);
    let mut padded = BytesMut::with_capacity(data * BODY);
    padded.put_u8(frame.codec as u8);
    padded.put_u64(frame.timestamp_us);
    padded.put_u32(frame.payload.len() as u32);
    frame.meta.encode(&mut padded);
    padded.extend_from_slice(&frame.payload);
    padded.resize(data * BODY, 0);
    let mut output = Vec::with_capacity(data + parity(data));
    for (group, chunk) in padded.chunks(GROUP * BODY).enumerate() {
        let count = chunk.len() / BODY;
        let mut pieces: Vec<_> = chunk.chunks(BODY).map(<[u8]>::to_vec).collect();
        pieces.resize_with(count + parity(count), || vec![0; BODY]);
        coder(count).encode(&mut pieces)?;
        for (index, piece) in pieces.iter().enumerate() {
            let mut bytes = BytesMut::with_capacity(HEADER + BODY);
            bytes.put_u8(ChannelId::Video as u8);
            bytes.put_u8(u8::from(frame.is_keyframe));
            bytes.put_u32(frame.frame_id);
            bytes.put_u16(group as u16);
            bytes.put_u16(index as u16);
            bytes.put_u16(data as u16);
            bytes.extend_from_slice(piece);
            output.push(bytes.freeze());
        }
    }
    Ok(output)
}

struct Assembly {
    started: Instant,
    keyframe: bool,
    data: usize,
    groups_remaining: usize,
    groups: Vec<Vec<Option<Vec<u8>>>>,
    recovered: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VideoCodec;

    #[tokio::test]
    async fn paced_join_keyframe_survives_an_encoder_stall_and_a_slow_link() -> Result<()> {
        let sender = UdpSocket::bind("127.0.0.1:0").await?;
        let receiver = UdpSocket::bind("127.0.0.1:0").await?;
        let sender_addr = sender.local_addr()?;
        let receiver_addr = receiver.local_addr()?;
        let (host, viewer) = tokio::try_join!(
            RawLink::new(sender, receiver_addr, [1; 32], [2; 32], true),
            RawLink::new(receiver, sender_addr, [2; 32], [1; 32], false),
        )?;
        let (mut host, mut viewer) = (host, viewer);
        let payload = Bytes::from(vec![0x65; 450_000]);
        let frame = OutboundVideoFrame {
            meta: FrameTelemetry::full_range_hardware(2, 1920, 1080),
            codec: VideoCodec::H264,
            frame_id: 1,
            timestamp_us: 0,
            captured_at: Instant::now() - Duration::from_millis(100),
            fps: 60,
            is_keyframe: true,
            payload: payload.clone(),
        };
        assert!(host.video(frame, 1_700_000).await?);
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    outgoing = host.next() => {
                        assert!(!matches!(outgoing?, Incoming::Keyframe), "sender expired a paced keyframe");
                    }
                    incoming = viewer.next() => match incoming? {
                        Incoming::Video(frame) => return Ok::<_, anyhow::Error>(frame),
                        Incoming::Keyframe => bail!("viewer expired a paced keyframe"),
                        Incoming::Packet(_) => {}
                    }
                }
            }
        }).await??;
        assert_eq!(delivered.payload, payload);
        Ok(())
    }

    #[test]
    fn raw_shards_recover_two_lost_data_packets_per_full_group() -> Result<()> {
        let payload = Bytes::from(
            (0..(BODY * 35))
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let frame = OutboundVideoFrame {
            meta: FrameTelemetry::full_range_hardware(1, 1920, 1080),
            codec: VideoCodec::Hevc,
            frame_id: 7,
            timestamp_us: 900,
            captured_at: Instant::now(),
            fps: 60,
            is_keyframe: true,
            payload: payload.clone(),
        };
        let mut recovered = Assembly::new((payload.len() + FRAME_INFO).div_ceil(BODY), true);
        for datagram in shards(frame)? {
            let group = usize::from(u16::from_be_bytes(datagram[6..8].try_into()?));
            let shard = usize::from(u16::from_be_bytes(datagram[8..10].try_into()?));
            if group < 2 && matches!(shard, 3 | 8) {
                continue;
            }
            let count = (recovered.data - group * GROUP).min(GROUP);
            recovered.put(group, count, shard, &datagram[HEADER..])?;
        }
        assert_eq!(recovered.groups_remaining, 0);
        let assembled = recovered.frame(7)?.unwrap();
        assert_eq!(assembled.fec_recovered_shards, 4);
        assert_eq!(assembled.payload, payload);
        Ok(())
    }
}

impl Assembly {
    fn new(data: usize, keyframe: bool) -> Self {
        let groups = data.div_ceil(GROUP);
        Self {
            data,
            keyframe,
            groups_remaining: groups,
            groups: vec![vec![]; groups],
            started: Instant::now(),
            recovered: 0,
        }
    }

    fn age(&self) -> Duration {
        if self.keyframe {
            KEYFRAME_AGE
        } else {
            DELTA_AGE
        }
    }

    fn missing(&self) -> usize {
        self.groups
            .iter()
            .enumerate()
            .map(|(index, group)| {
                let count = (self.data - index * GROUP).min(GROUP);
                count - group.iter().take(count).flatten().count()
            })
            .sum()
    }

    fn put(&mut self, group: usize, count: usize, shard: usize, bytes: &[u8]) -> Result<()> {
        let parts = &mut self.groups[group];
        if parts.is_empty() {
            parts.resize_with(count + parity(count), || None);
        }
        if parts[..count].iter().all(Option::is_some) {
            return Ok(());
        }
        if parts[shard].is_none() {
            parts[shard] = Some(bytes.to_vec());
        }
        if parts.iter().flatten().count() >= count {
            let missing = parts[..count]
                .iter()
                .filter(|piece| piece.is_none())
                .count();
            if missing > 0 {
                coder(count).reconstruct_data(parts)?;
            }
            self.recovered += missing as u32;
            self.groups_remaining -= 1;
        }
        Ok(())
    }

    fn frame(self, frame_id: u32) -> Result<Option<ReassembledFrame>> {
        let mut bytes = BytesMut::with_capacity(self.data * BODY);
        for (index, group) in self.groups.into_iter().enumerate() {
            let count = (self.data - index * GROUP).min(GROUP);
            for part in group.into_iter().take(count).flatten() {
                bytes.extend_from_slice(&part);
            }
        }
        let (codec, timestamp_us) = (bytes[0], u64::from_be_bytes(bytes[1..9].try_into()?));
        let len = u32::from_be_bytes(bytes[9..13].try_into()?) as usize;
        let Some(meta) = FrameTelemetry::decode(&bytes[13..FRAME_INFO]) else {
            return Ok(None);
        };
        if len > bytes.len() - FRAME_INFO {
            return Ok(None);
        }
        Ok(Some(ReassembledFrame {
            frame_id,
            codec,
            timestamp_us,
            is_keyframe: self.keyframe,
            fec_recovered_shards: self.recovered,
            meta,
            payload: bytes.freeze().slice(FRAME_INFO..FRAME_INFO + len),
        }))
    }
}
