use bytes::{Buf, BufMut, Bytes, BytesMut};
use reed_solomon_erasure::galois_8::{div, mul, mul_slice_xor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
/// Fixed video header: channel/flags/codec/fec (4) + frame_id u64 (8) +
/// timestamp u64 (8) + chunk_idx u16 (2) + total u16 (2) + the 16-byte
/// telemetry record. See [`crate::telemetry`].
pub const VIDEO_HEADER_LEN: usize = 40;
pub const AUDIO_HEADER_LEN: usize = 16;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelId {
    Video = 0x01,
    Audio = 0x02,
    Input = 0x03,
    Control = 0x04,
    /// Fixed-size delay-gradient / NACK report (see `congestion::FeedbackPacket`).
    Feedback = 0x05,
    Haptics = 0x06,
}

impl TryFrom<u8> for ChannelId {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Video),
            0x02 => Ok(Self::Audio),
            0x03 => Ok(Self::Input),
            0x04 => Ok(Self::Control),
            0x05 => Ok(Self::Feedback),
            0x06 => Ok(Self::Haptics),
            other => Err(ProtocolError::UnknownChannel(other)),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    InputReliable = 0x10,
    ControlReliable = 0x11,
}

impl TryFrom<u8> for StreamKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x10 => Ok(Self::InputReliable),
            0x11 => Ok(Self::ControlReliable),
            other => Err(ProtocolError::UnknownStreamKind(other)),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("packet too short: expected at least {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },
    #[error("unknown channel id: {0:#x}")]
    UnknownChannel(u8),
    #[error("unknown stream kind: {0:#x}")]
    UnknownStreamKind(u8),
    #[error("unknown input event tag: {0:#x}")]
    UnknownInputTag(u8),
    #[error("unknown touch action {0}")]
    UnknownTouchAction(u8),
    #[error("invalid max datagram size {0}: too small for VmaX header")]
    DatagramTooSmall(usize),
    #[error("invalid frame chunk layout")]
    InvalidChunkLayout,
    #[error("JSON control serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

pub mod video_flags {
    pub const KEYFRAME: u8 = 1 << 0;
    pub const PARITY: u8 = 1 << 1;
    pub const END_OF_FRAME: u8 = 1 << 2;
    pub const WEIGHTED_PARITY: u8 = 1 << 3;
}

pub mod audio_flags {
    pub const DENOISED: u8 = 1 << 0;
    pub const VOICE_TRACK: u8 = 1 << 1;
    pub const SYSTEM_TRACK: u8 = 1 << 2;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    Hevc = 1,
    H264 = 2,
    HevcMain10 = 3,
    #[serde(rename = "hevc_444")]
    Hevc444 = 4,
}

impl VideoCodec {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Hevc),
            2 => Some(Self::H264),
            3 => Some(Self::HevcMain10),
            4 => Some(Self::Hevc444),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus = 1,
    PcmF32 = 2,
    PcmI16 = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoChunkHeader {
    pub flags: u8,
    pub codec: u8,
    pub fec_group_size: u8,
    /// 64-bit so a long session cannot alias a frame id after a wrap.
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub chunk_idx: u16,
    pub total_data_chunks: u16,
    /// Host encode timing and stream description, repeated on every chunk so a
    /// mid-stream client still learns it. See [`crate::telemetry`].
    pub meta: crate::telemetry::FrameTelemetry,
}

impl VideoChunkHeader {
    pub fn encode(&self, out: &mut BytesMut) {
        out.put_u8(ChannelId::Video as u8);
        out.put_u8(self.flags);
        out.put_u8(self.codec);
        out.put_u8(self.fec_group_size);
        out.put_u64(self.frame_id);
        out.put_u64(self.timestamp_us);
        out.put_u16(self.chunk_idx);
        out.put_u16(self.total_data_chunks);
        self.meta.encode(out);
    }

    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if buf.len() < VIDEO_HEADER_LEN {
            return Err(ProtocolError::Truncated {
                expected: VIDEO_HEADER_LEN,
                actual: buf.len(),
            });
        }
        if buf[0] != ChannelId::Video as u8 {
            return Err(ProtocolError::UnknownChannel(buf[0]));
        }
        let header = Self {
            flags: buf[1],
            codec: buf[2],
            fec_group_size: buf[3],
            frame_id: u64::from_be_bytes([
                buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
            ]),
            timestamp_us: u64::from_be_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ]),
            chunk_idx: u16::from_be_bytes([buf[20], buf[21]]),
            total_data_chunks: u16::from_be_bytes([buf[22], buf[23]]),
            meta: crate::telemetry::FrameTelemetry::decode(&buf[24..VIDEO_HEADER_LEN])
                .ok_or(ProtocolError::Truncated {
                    expected: VIDEO_HEADER_LEN,
                    actual: buf.len(),
                })?,
        };
        Ok((header, &buf[VIDEO_HEADER_LEN..]))
    }

    #[inline]
    pub fn is_parity(&self) -> bool {
        (self.flags & video_flags::PARITY) != 0
    }

    #[inline]
    pub fn is_keyframe(&self) -> bool {
        (self.flags & video_flags::KEYFRAME) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPacketHeader {
    pub codec: u8,
    pub flags: u8,
    pub channels: u8,
    pub seq: u32,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub header: AudioPacketHeader,
    pub payload: Bytes,
}

impl AudioPacket {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        buf.put_u8(ChannelId::Audio as u8);
        buf.put_u8(self.header.codec);
        buf.put_u8(self.header.flags);
        buf.put_u8(self.header.channels);
        buf.put_u32(self.header.seq);
        buf.put_u64(self.header.timestamp_us);
        buf.extend_from_slice(&self.payload);
        buf.freeze()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(ProtocolError::Truncated {
                expected: AUDIO_HEADER_LEN,
                actual: buf.len(),
            });
        }
        if buf[0] != ChannelId::Audio as u8 {
            return Err(ProtocolError::UnknownChannel(buf[0]));
        }
        let header = AudioPacketHeader {
            codec: buf[1],
            flags: buf[2],
            channels: buf[3],
            seq: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            timestamp_us: u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
        };
        Ok(Self {
            header,
            payload: Bytes::copy_from_slice(&buf[AUDIO_HEADER_LEN..]),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum TouchAction {
    Tap = 0,
    Down = 1,
    Up = 2,
    Context = 3,
    Drag = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    MouseMoveRel {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbs {
        x_norm: f32,
        y_norm: f32,
    },
    TouchPointer {
        x_norm: f32,
        y_norm: f32,
        action: TouchAction,
        tap_id: u32,
    },
    Unicode {
        codepoint: u32,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    MouseWheel {
        delta_x: i16,
        delta_y: i16,
    },
    Key {
        scan_code: u32,
        pressed: bool,
        modifiers: u8,
    },
    GamepadState {
        index: u8,
        buttons: u16,
        left_trigger: u8,
        right_trigger: u8,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputPacket {
    pub seq: u64,
    pub client_ts_us: u64,
    pub event: InputEvent,
}

impl InputPacket {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(40);
        buf.put_u8(ChannelId::Input as u8);
        buf.put_u64(self.seq);
        buf.put_u64(self.client_ts_us);
        match self.event {
            InputEvent::MouseMoveRel { dx, dy } => {
                buf.put_u8(0x01);
                buf.put_i32(dx);
                buf.put_i32(dy);
            }
            InputEvent::MouseMoveAbs { x_norm, y_norm } => {
                buf.put_u8(0x02);
                buf.put_f32(x_norm);
                buf.put_f32(y_norm);
            }
            InputEvent::MouseButton { button, pressed } => {
                buf.put_u8(0x03);
                buf.put_u8(button);
                buf.put_u8(u8::from(pressed));
            }
            InputEvent::MouseWheel { delta_x, delta_y } => {
                buf.put_u8(0x04);
                buf.put_i16(delta_x);
                buf.put_i16(delta_y);
            }
            InputEvent::Key {
                scan_code,
                pressed,
                modifiers,
            } => {
                buf.put_u8(0x05);
                buf.put_u32(scan_code);
                buf.put_u8(u8::from(pressed));
                buf.put_u8(modifiers);
            }
            InputEvent::GamepadState {
                index,
                buttons,
                left_trigger,
                right_trigger,
                left_x,
                left_y,
                right_x,
                right_y,
            } => {
                buf.put_u8(0x06);
                buf.put_u8(index);
                buf.put_u16(buttons);
                buf.put_u8(left_trigger);
                buf.put_u8(right_trigger);
                buf.put_i16(left_x);
                buf.put_i16(left_y);
                buf.put_i16(right_x);
                buf.put_i16(right_y);
            }
            InputEvent::TouchPointer {
                x_norm,
                y_norm,
                action,
                tap_id,
            } => {
                buf.put_u8(0x07);
                buf.put_f32(x_norm);
                buf.put_f32(y_norm);
                buf.put_u8(action as u8);
                buf.put_u32(tap_id);
            }
            InputEvent::Unicode { codepoint } => {
                buf.put_u8(0x08);
                buf.put_u32(codepoint);
            }
        }
        buf.freeze()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < 18 {
            return Err(ProtocolError::Truncated {
                expected: 18,
                actual: buf.len(),
            });
        }
        if buf[0] != ChannelId::Input as u8 {
            return Err(ProtocolError::UnknownChannel(buf[0]));
        }
        let seq = u64::from_be_bytes([
            buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8],
        ]);
        let client_ts_us = u64::from_be_bytes([
            buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16],
        ]);
        let tag = buf[17];
        let body = &buf[18..];
        let need = |n: usize| -> Result<(), ProtocolError> {
            if body.len() < n {
                Err(ProtocolError::Truncated {
                    expected: 18 + n,
                    actual: buf.len(),
                })
            } else {
                Ok(())
            }
        };

        let event = match tag {
            0x01 => {
                need(8)?;
                InputEvent::MouseMoveRel {
                    dx: i32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                    dy: i32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                }
            }
            0x02 => {
                need(8)?;
                InputEvent::MouseMoveAbs {
                    x_norm: f32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                    y_norm: f32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                }
            }
            0x03 => {
                need(2)?;
                InputEvent::MouseButton {
                    button: body[0],
                    pressed: body[1] != 0,
                }
            }
            0x04 => {
                need(4)?;
                InputEvent::MouseWheel {
                    delta_x: i16::from_be_bytes([body[0], body[1]]),
                    delta_y: i16::from_be_bytes([body[2], body[3]]),
                }
            }
            0x05 => {
                need(6)?;
                InputEvent::Key {
                    scan_code: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                    pressed: body[4] != 0,
                    modifiers: body[5],
                }
            }
            0x06 => {
                need(13)?;
                InputEvent::GamepadState {
                    index: body[0],
                    buttons: u16::from_be_bytes([body[1], body[2]]),
                    left_trigger: body[3],
                    right_trigger: body[4],
                    left_x: i16::from_be_bytes([body[5], body[6]]),
                    left_y: i16::from_be_bytes([body[7], body[8]]),
                    right_x: i16::from_be_bytes([body[9], body[10]]),
                    right_y: i16::from_be_bytes([body[11], body[12]]),
                }
            }
            0x07 => {
                need(13)?;
                let mut body = body;
                let x_norm = body.get_f32();
                let y_norm = body.get_f32();
                let action = match body.get_u8() {
                    0 => TouchAction::Tap,
                    1 => TouchAction::Down,
                    2 => TouchAction::Up,
                    3 => TouchAction::Context,
                    4 => TouchAction::Drag,
                    other => return Err(ProtocolError::UnknownTouchAction(other)),
                };
                InputEvent::TouchPointer {
                    x_norm,
                    y_norm,
                    action,
                    tap_id: body.get_u32(),
                }
            }
            0x08 => {
                need(4)?;
                InputEvent::Unicode {
                    codepoint: (&body[..4]).get_u32(),
                }
            }
            other => return Err(ProtocolError::UnknownInputTag(other)),
        };

        Ok(Self {
            seq,
            client_ts_us,
            event,
        })
    }

    pub fn is_high_frequency_motion(&self) -> bool {
        matches!(
            self.event,
            InputEvent::MouseMoveRel { .. } | InputEvent::MouseMoveAbs { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Hello {
        protocol_version: u16,
        role: String,
        max_bitrate_bps: u32,
        target_fps: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_key: Option<String>,
        #[serde(default)]
        two_shard_fec: bool,
    },
    Ping {
        client_ts_us: u64,
    },
    Pong {
        client_ts_us: u64,
        peer_ts_us: u64,
    },
    RequestKeyframe {
        last_good_frame_id: Option<u32>,
        reason: String,
    },
    EncoderLatency {
        micros: u32,
    },
    ClientBitrateLimit {
        mbps: u32,
    },
    ReconnectGamepad,
    GamepadPort {
        port: Option<u8>,
    },
    BitrateUpdate {
        target_bitrate_bps: u32,
        reason: String,
    },
    TelemetryFeedback {
        rtt_ms: f64,
        loss_pct: f64,
        frames_received: u64,
        frames_dropped: u64,
        fec_recovered: u64,
        target_bitrate_bps: u32,
    },
    ClientCapabilities {
        preferred_codecs: Vec<VideoCodec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<crate::handshake::ClientDisplay>,
    },
    TextFocus {
        tap_id: u32,
        editable: bool,
    },
    CursorPosition {
        x_norm: f32,
        y_norm: f32,
        visible: bool,
    },
    StreamConfig {
        width: u16,
        height: u16,
        fps: u16,
        codec: VideoCodec,
        bitrate_mbps: u32,
        fec_pct: u8,
        hardware_cursor: bool,
    },
    Disconnect {
        reason: String,
    },
}

impl ControlMessage {
    pub fn encode_datagram(&self) -> Result<Bytes, ProtocolError> {
        let json = serde_json::to_vec(self)?;
        let mut out = BytesMut::with_capacity(1 + json.len());
        out.put_u8(ChannelId::Control as u8);
        out.extend_from_slice(&json);
        Ok(out.freeze())
    }

    pub fn decode_datagram(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.is_empty() {
            return Err(ProtocolError::Truncated {
                expected: 1,
                actual: 0,
            });
        }
        if buf[0] != ChannelId::Control as u8 {
            return Err(ProtocolError::UnknownChannel(buf[0]));
        }
        Ok(serde_json::from_slice(&buf[1..])?)
    }

    pub fn encode_framed(&self) -> Result<Bytes, ProtocolError> {
        let json = serde_json::to_vec(self)?;
        let mut out = BytesMut::with_capacity(4 + json.len());
        out.put_u32(json.len() as u32);
        out.extend_from_slice(&json);
        Ok(out.freeze())
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

pub fn packetize_video_frame(
    frame_id: u32,
    timestamp_us: u64,
    is_keyframe: bool,
    payload: &[u8],
    max_datagram_size: usize,
    fec_group_size: u8,
) -> Result<Vec<Bytes>, ProtocolError> {
    packetize_video_frame_with_codec(
        VideoCodec::Hevc,
        frame_id,
        timestamp_us,
        is_keyframe,
        payload,
        max_datagram_size,
        fec_group_size,
    )
}

pub fn packetize_video_frame_with_codec(
    codec: VideoCodec,
    frame_id: u32,
    timestamp_us: u64,
    is_keyframe: bool,
    payload: &[u8],
    max_datagram_size: usize,
    fec_group_size: u8,
) -> Result<Vec<Bytes>, ProtocolError> {
    packetize_video(
        codec,
        u64::from(frame_id),
        timestamp_us,
        is_keyframe,
        payload,
        max_datagram_size,
        fec_group_size,
        false,
        crate::telemetry::FrameTelemetry::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn packetize_video(
    codec: VideoCodec,
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    payload: &[u8],
    max_datagram_size: usize,
    fec_group_size: u8,
    two_shard_fec: bool,
    meta: crate::telemetry::FrameTelemetry,
) -> Result<Vec<Bytes>, ProtocolError> {
    let _ = VIDEO_HEADER_LEN;
    if max_datagram_size <= VIDEO_HEADER_LEN + 16 {
        return Err(ProtocolError::DatagramTooSmall(max_datagram_size));
    }
    let max_data_payload = max_datagram_size - VIDEO_HEADER_LEN - 2;
    let total_data_chunks = payload.chunks(max_data_payload).len().max(1);
    if total_data_chunks > u16::MAX as usize {
        return Err(ProtocolError::InvalidChunkLayout);
    }

    let total_chunks_u16 = total_data_chunks as u16;
    let mut packets = Vec::with_capacity(
        total_data_chunks
            + if fec_group_size > 0 {
                total_data_chunks.div_ceil(fec_group_size as usize)
                    * if two_shard_fec { 2 } else { 1 }
            } else {
                0
            },
    );

    let data_slices: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(max_data_payload).collect()
    };

    let codec_byte = codec as u8;

    for (idx, chunk) in data_slices.iter().enumerate() {
        let mut flags = if is_keyframe {
            video_flags::KEYFRAME
        } else {
            0
        };
        if idx + 1 == total_data_chunks {
            flags |= video_flags::END_OF_FRAME;
        }
        let header = VideoChunkHeader {
            flags,
            codec: codec_byte,
            fec_group_size,
            frame_id,
            timestamp_us,
            chunk_idx: idx as u16,
            total_data_chunks: total_chunks_u16,
            meta,
        };
        let mut pkt = BytesMut::with_capacity(VIDEO_HEADER_LEN + chunk.len());
        header.encode(&mut pkt);
        pkt.extend_from_slice(chunk);
        packets.push(pkt.freeze());
    }

    if fec_group_size > 0 {
        let k = fec_group_size as usize;
        for (block_idx, group) in data_slices.chunks(k).enumerate() {
            let block_max_len = group.iter().map(|s| s.len()).max().unwrap_or(0);
            let mut xor_buf = vec![0u8; 2 + block_max_len];
            let mut weighted =
                (two_shard_fec && group.len() > (k / 2).max(1)).then(|| vec![0u8; xor_buf.len()]);
            for (index, shard) in group.iter().enumerate() {
                accumulate_parity(&mut xor_buf, shard, 1);
                if let Some(weighted) = &mut weighted {
                    accumulate_parity(weighted, shard, (index + 1) as u8);
                }
            }
            let mut flags = video_flags::PARITY;
            if is_keyframe {
                flags |= video_flags::KEYFRAME;
            }
            let mut header = VideoChunkHeader {
                flags,
                codec: codec_byte,
                fec_group_size,
                frame_id,
                timestamp_us,
                chunk_idx: block_idx as u16,
                total_data_chunks: total_chunks_u16,
                meta,
            };
            let mut pkt = BytesMut::with_capacity(VIDEO_HEADER_LEN + xor_buf.len());
            header.encode(&mut pkt);
            pkt.extend_from_slice(&xor_buf);
            packets.push(pkt.freeze());
            if let Some(weighted) = weighted {
                header.flags |= video_flags::WEIGHTED_PARITY;
                let mut pkt = BytesMut::with_capacity(VIDEO_HEADER_LEN + weighted.len());
                header.encode(&mut pkt);
                pkt.extend_from_slice(&weighted);
                packets.push(pkt.freeze());
            }
        }
    }

    Ok(packets)
}

#[cfg(test)]
mod fec_tests {
    use super::*;

    #[test]
    fn longer_recovery_window_keeps_a_late_fragment() {
        let payload = [7; 1_500];
        let packets = packetize_video_frame(1, 0, false, &payload, 1_200, 0).unwrap();
        let receive = |window| {
            let mut receiver = VideoReassembler::new(window);
            receiver.push_datagram(&packets[0]).unwrap();
            receiver.in_flight.get_mut(&1).unwrap().created_at -= Duration::from_millis(45);
            receiver.push_datagram(&packets[1]).unwrap()
        };
        assert!(matches!(
            receive(Duration::from_millis(32)),
            ReassembleOutcome::DroppedNeedKeyframe { dropped_frames: 1 }
        ));
        assert!(
            matches!(receive(Duration::from_millis(60)), ReassembleOutcome::Completed { frame, dropped_frames: 0 } if frame.payload.as_ref() == payload)
        );
    }

    #[test]
    fn completion_reports_incomplete_frames_it_supersedes_or_expires() {
        let window = Duration::from_millis(25);
        let mut receiver = VideoReassembler::new(window);
        for id in [1, 2] {
            let packets = packetize_video_frame(id, 0, false, &[1; 1_500], 1_200, 0).unwrap();
            assert!(matches!(
                receiver.push_datagram(&packets[0]).unwrap(),
                ReassembleOutcome::Incomplete
            ));
        }
        receiver.in_flight.get_mut(&1).unwrap().created_at -= window;
        let keyframe = packetize_video_frame(3, 0, true, &[2; 100], 1_200, 0).unwrap();
        assert!(matches!(
            receiver.push_datagram(&keyframe[0]).unwrap(),
            ReassembleOutcome::Completed { frame, dropped_frames: 2 } if frame.frame_id == 3
        ));
    }

    #[test]
    fn equal_overhead_fec_recovers_all_two_datagram_losses_in_a_group() {
        let payload: Vec<u8> = (0..11_400).map(|i| (i % 251) as u8).collect();
        let count_recoveries = |group, two_shards| {
            let packets = packetize_video(
                VideoCodec::H264,
                42,
                1234,
                true,
                &payload,
                1_200,
                group,
                two_shards,
                crate::telemetry::FrameTelemetry::default(),
            )
            .unwrap();
            assert_eq!(packets.len(), 12);
            let mut recovered = 0;
            for a in 0..packets.len() {
                for b in a + 1..packets.len() {
                    let mut receiver = VideoReassembler::new(Duration::from_secs(1));
                    for index in (0..10).rev().chain(10..packets.len()) {
                        let packet = &packets[index];
                        if index != a
                            && index != b
                            && let ReassembleOutcome::Completed { frame, .. } =
                                receiver.push_datagram(packet).unwrap()
                        {
                            assert_eq!(frame.payload.as_ref(), payload);
                            assert_eq!(
                                frame.fec_recovered_shards,
                                u32::from(a < 10) + u32::from(b < 10)
                            );
                            recovered += 1;
                        }
                    }
                }
            }
            recovered
        };
        assert_eq!(count_recoveries(5, false), 36);
        assert_eq!(count_recoveries(10, true), 66);
    }

    #[test]
    fn two_shard_fec_preserves_the_parity_packet_budget_on_partial_groups() {
        for data_chunks in 1..=25 {
            let payload = vec![1; data_chunks * (1_200 - VIDEO_HEADER_LEN - 2)];
            let legacy = packetize_video_frame(1, 0, false, &payload, 1_200, 5).unwrap();
            let upgraded =
                packetize_video(
                    VideoCodec::Hevc,
                    1,
                    0,
                    false,
                    &payload,
                    1_200,
                    10,
                    true,
                    crate::telemetry::FrameTelemetry::default(),
                )
                .unwrap();
            assert_eq!(legacy.len(), upgraded.len());
            assert_eq!(
                legacy.iter().map(Bytes::len).sum::<usize>(),
                upgraded.iter().map(Bytes::len).sum::<usize>()
            );
        }
    }

    #[test]
    fn older_hello_messages_default_to_single_shard_fec() {
        let hello = br#"{"type":"hello","protocol_version":1,"role":"Client","max_bitrate_bps":20000000,"target_fps":60}"#;
        assert!(matches!(
            ControlMessage::decode_json(hello).unwrap(),
            ControlMessage::Hello {
                two_shard_fec: false,
                ..
            }
        ));
    }
}

fn accumulate_parity(parity: &mut [u8], shard: &[u8], coefficient: u8) {
    for (dst, src) in parity[..2]
        .iter_mut()
        .zip((shard.len() as u16).to_be_bytes())
    {
        *dst ^= mul(coefficient, src);
    }
    mul_slice_xor(coefficient, shard, &mut parity[2..2 + shard.len()]);
}

#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    /// 32-bit view of the wire's 64-bit id. Consumers (jitter buffer, Android
    /// bridge) key on this and it is monotonic within a session.
    pub frame_id: u32,
    pub codec: u8,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
    pub fec_recovered_shards: u32,
    pub payload: Bytes,
    /// Host encode timing and stream description for this frame.
    pub meta: crate::telemetry::FrameTelemetry,
}

#[derive(Debug, Clone)]
pub enum ReassembleOutcome {
    Incomplete,
    Completed {
        frame: ReassembledFrame,
        dropped_frames: u32,
    },
    DroppedNeedKeyframe {
        dropped_frames: u32,
    },
}

struct FrameAssembly {
    codec: u8,
    meta: crate::telemetry::FrameTelemetry,
    timestamp_us: u64,
    is_keyframe: bool,
    fec_group_size: u8,
    total_data_chunks: usize,
    data_chunks: Vec<Option<Bytes>>,
    received_data_count: usize,
    parity_chunks: BTreeMap<usize, [Option<Bytes>; 2]>,
    fec_recovered_shards: u32,
    created_at: Instant,
}

impl FrameAssembly {
    fn new(header: &VideoChunkHeader) -> Self {
        let total = header.total_data_chunks as usize;
        Self {
            codec: header.codec,
            meta: header.meta,
            timestamp_us: header.timestamp_us,
            is_keyframe: header.is_keyframe(),
            fec_group_size: header.fec_group_size,
            total_data_chunks: total,
            data_chunks: vec![None; total],
            received_data_count: 0,
            parity_chunks: BTreeMap::new(),
            fec_recovered_shards: 0,
            created_at: Instant::now(),
        }
    }

    fn insert_packet(&mut self, header: &VideoChunkHeader, payload: &[u8]) {
        let k = self.fec_group_size as usize;
        let idx = header.chunk_idx as usize;
        let block = if header.is_parity() {
            if k == 0 || idx >= self.total_data_chunks.div_ceil(k) || payload.len() < 2 {
                return;
            }
            let parity = usize::from(header.flags & video_flags::WEIGHTED_PARITY != 0);
            self.parity_chunks.entry(idx).or_default()[parity]
                .get_or_insert_with(|| Bytes::copy_from_slice(payload));
            idx
        } else {
            if idx >= self.total_data_chunks || self.data_chunks[idx].is_some() {
                return;
            }
            self.data_chunks[idx] = Some(Bytes::copy_from_slice(payload));
            self.received_data_count += 1;
            idx / k.max(1)
        };
        self.try_fec_recover(block);
    }

    fn try_fec_recover(&mut self, block: usize) {
        let Some(parity) = self.parity_chunks.get(&block) else {
            return;
        };
        let k = self.fec_group_size as usize;
        let start = block * k;
        let end = (start + k).min(self.total_data_chunks);
        let group = &mut self.data_chunks[start..end];
        let mut missing = group
            .iter()
            .enumerate()
            .filter(|(_, shard)| shard.is_none());
        let Some((first, _)) = missing.next() else {
            return;
        };
        let second = missing.next().map(|(index, _)| index);
        if missing.next().is_some() {
            return;
        }
        let residual = |parity: &Bytes, weighted| {
            let mut residual = parity.to_vec();
            for (index, shard) in group.iter().enumerate() {
                if let Some(shard) = shard {
                    if shard.len() > residual.len() - 2 {
                        return None;
                    }
                    accumulate_parity(
                        &mut residual,
                        shard,
                        if weighted { (index + 1) as u8 } else { 1 },
                    );
                }
            }
            Some(residual)
        };
        let first_coefficient = (first + 1) as u8;
        let (first_shard, second_shard) = match (&parity[0], &parity[1], second) {
            (Some(xor), Some(weighted), Some(second)) if xor.len() == weighted.len() => {
                let (Some(mut a), Some(mut b)) = (residual(xor, false), residual(weighted, true))
                else {
                    return;
                };
                let second_coefficient = (second + 1) as u8;
                for (x, y) in a.iter_mut().zip(b.iter_mut()) {
                    let recovered = div(
                        *y ^ mul(second_coefficient, *x),
                        first_coefficient ^ second_coefficient,
                    );
                    *y = *x ^ recovered;
                    *x = recovered;
                }
                (a, Some((second, b)))
            }
            (Some(xor), _, None) => {
                let Some(shard) = residual(xor, false) else {
                    return;
                };
                (shard, None)
            }
            (None, Some(weighted), None) => {
                let Some(mut shard) = residual(weighted, true) else {
                    return;
                };
                for byte in &mut shard {
                    *byte = div(*byte, first_coefficient);
                }
                (shard, None)
            }
            _ => return,
        };

        for (index, shard) in std::iter::once((first, first_shard)).chain(second_shard) {
            let len = u16::from_be_bytes([shard[0], shard[1]]) as usize;
            if 2 + len <= shard.len() {
                group[index] = Some(Bytes::from(shard).slice(2..2 + len));
                self.received_data_count += 1;
                self.fec_recovered_shards += 1;
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.total_data_chunks > 0 && self.received_data_count == self.total_data_chunks
    }

    fn assemble(self, frame_id: u64) -> ReassembledFrame {
        let total_bytes: usize = self
            .data_chunks
            .iter()
            .filter_map(|c| c.as_ref().map(|b| b.len()))
            .sum();
        let mut out = BytesMut::with_capacity(total_bytes);
        for chunk in self.data_chunks.into_iter().flatten() {
            out.extend_from_slice(&chunk);
        }
        ReassembledFrame {
            frame_id: frame_id as u32,
            codec: self.codec,
            timestamp_us: self.timestamp_us,
            is_keyframe: self.is_keyframe,
            fec_recovered_shards: self.fec_recovered_shards,
            payload: out.freeze(),
            meta: self.meta,
        }
    }
}

pub struct VideoReassembler {
    in_flight: BTreeMap<u64, FrameAssembly>,
    last_completed_frame: Option<u64>,
    max_jitter_window: Duration,
    keyframe_jitter_window: Duration,
    max_in_flight: usize,
}

impl VideoReassembler {
    pub fn new(max_jitter_window: Duration) -> Self {
        Self {
            in_flight: BTreeMap::new(),
            last_completed_frame: None,
            max_jitter_window,
            keyframe_jitter_window: max_jitter_window,
            max_in_flight: 8,
        }
    }

    pub(crate) fn set_jitter_windows(&mut self, delta: Duration, keyframe: Duration) {
        self.max_jitter_window = delta;
        self.keyframe_jitter_window = keyframe.max(delta);
    }

    pub fn push_datagram(&mut self, datagram: &[u8]) -> Result<ReassembleOutcome, ProtocolError> {
        let (header, payload) = VideoChunkHeader::decode(datagram)?;
        if header.total_data_chunks == 0 {
            return Err(ProtocolError::InvalidChunkLayout);
        }

        if let Some(last) = self.last_completed_frame
            && header.frame_id <= last
        {
            return Ok(ReassembleOutcome::Incomplete);
        }

        let dropped_from_sweep = self.evict_expired();

        let entry = self
            .in_flight
            .entry(header.frame_id)
            .or_insert_with(|| FrameAssembly::new(&header));
        if entry.total_data_chunks != usize::from(header.total_data_chunks)
            || entry.fec_group_size != header.fec_group_size
            || entry.codec != header.codec
            || entry.is_keyframe != header.is_keyframe()
            || entry.timestamp_us != header.timestamp_us
        {
            return Err(ProtocolError::InvalidChunkLayout);
        }
        entry.insert_packet(&header, payload);

        if entry.is_complete() {
            let Some(completed) = self.in_flight.remove(&header.frame_id) else {
                return Ok(ReassembleOutcome::Incomplete);
            };
            let stale_ids: Vec<u64> = self
                .in_flight
                .keys()
                .copied()
                .filter(|id| *id < header.frame_id)
                .collect();
            let dropped_frames = dropped_from_sweep + stale_ids.len() as u32;
            for id in stale_ids {
                self.in_flight.remove(&id);
            }
            self.last_completed_frame = Some(header.frame_id);
            return Ok(ReassembleOutcome::Completed {
                frame: completed.assemble(header.frame_id),
                dropped_frames,
            });
        }

        if dropped_from_sweep > 0 {
            return Ok(ReassembleOutcome::DroppedNeedKeyframe {
                dropped_frames: dropped_from_sweep,
            });
        }

        Ok(ReassembleOutcome::Incomplete)
    }

    pub fn evict_expired(&mut self) -> u32 {
        let now = Instant::now();
        let expired: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, frame)| {
                let window = if frame.is_keyframe {
                    self.keyframe_jitter_window
                } else {
                    self.max_jitter_window
                };
                now.duration_since(frame.created_at) > window
            })
            .map(|(id, _)| *id)
            .collect();
        let mut dropped = expired.len() as u32;
        for id in expired {
            self.in_flight.remove(&id);
        }

        while self.in_flight.len() > self.max_in_flight {
            if let Some(oldest_id) = self.in_flight.keys().next().copied() {
                self.in_flight.remove(&oldest_id);
                dropped += 1;
            }
        }
        dropped
    }
}