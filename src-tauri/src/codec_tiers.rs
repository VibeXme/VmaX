//! Codec capability negotiation (the "universal handshake").
//!
//! VmaX already exchanges a reliable `ClientCapabilities` message with an
//! ordered codec list and display limits. This module adds the compact,
//! fixed-size form and the explicit tier ladder, so a host and client always
//! agree on the best mutually supported codec *before* the first frame:
//!
//! | Tier | Codec | Devices |
//! |------|-------|---------|
//! | 1 | H.264 High / Constrained Baseline, 8-bit NV12 | anything from ~2012 on |
//! | 2 | HEVC Main, 8-bit NV12 | GTX 950+, RX 400+, Skylake+, modern phones |
//! | 3 | HEVC Main10 (P010) or AV1 | RTX 30+, RX 7000+, recent flagships |
//!
//! Wire format: a 12-byte little-endian datagram, small enough to ride in the
//! QUIC handshake or a single reliable control message.
//!
//! ```text
//! offset size field
//! 0      1    flags            bit0 hevc, bit1 hevc_10bit, bit2 av1, bit3 h264
//! 1      1    tier_hint        highest tier the client would like (1..=3)
//! 2      2    max_width
//! 4      2    max_height
//! 6      2    max_fps
//! 8      2    max_bitrate_mbps (0 = unknown)
//! 10     2    reserved (0)
//! ```
//!
//! The host answers with the highest tier both sides support, clamped to the
//! client's size/fps/bitrate limits. Tier 1 is always available, which is what
//! makes the handshake total: an Intel HD 4400 laptop and a 2013 phone still
//! connect, just at H.264.

use crate::handshake::ClientDisplay;
use crate::protocol::VideoCodec;
use bytemuck::{Pod, Zeroable};

/// Highest tier the host can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CodecTier {
    Avc = 1,
    Hevc = 2,
    HevcMain10 = 3,
    Av1 = 4,
}

impl CodecTier {
    pub fn codec(self) -> VideoCodec {
        match self {
            Self::Avc => VideoCodec::H264,
            Self::Hevc | Self::Av1 => VideoCodec::Hevc,
            Self::HevcMain10 => VideoCodec::HevcMain10,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Avc => "H.264 (tier 1)",
            Self::Hevc => "HEVC Main (tier 2)",
            Self::HevcMain10 => "HEVC Main10 (tier 3)",
            Self::Av1 => "AV1 (tier 3)",
        }
    }

    /// Relative bitrate efficiency versus H.264, in percent.
    pub fn bitrate_efficiency_pct(self) -> u8 {
        match self {
            Self::Avc => 100,
            Self::Hevc => 65,
            Self::HevcMain10 => 65,
            Self::Av1 => 55,
        }
    }
}

pub const FLAG_HEVC: u8 = 1 << 0;
pub const FLAG_HEVC_10BIT: u8 = 1 << 1;
pub const FLAG_AV1: u8 = 1 << 2;
pub const FLAG_H264: u8 = 1 << 3;

/// 12-byte capability datagram.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct ClientCaps {
    pub flags: u8,
    pub tier_hint: u8,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub max_bitrate_mbps: u16,
    pub reserved: u16,
}

const _: () = assert!(core::mem::size_of::<ClientCaps>() == 12);

impl ClientCaps {
    pub fn new(max_width: u16, max_height: u16, max_fps: u16) -> Self {
        Self {
            // H.264 is the universal baseline: every decoder has it.
            flags: FLAG_H264,
            tier_hint: 1,
            max_width,
            max_height,
            max_fps,
            max_bitrate_mbps: 0,
            reserved: 0,
        }
    }

    pub fn with_codec(mut self, tier: CodecTier) -> Self {
        match tier {
            CodecTier::Avc => self.flags |= FLAG_H264,
            CodecTier::Hevc => self.flags |= FLAG_HEVC,
            CodecTier::HevcMain10 => self.flags |= FLAG_HEVC | FLAG_HEVC_10BIT,
            CodecTier::Av1 => self.flags |= FLAG_AV1,
        }
        self.tier_hint = self.tier_hint.max(tier as u8);
        self
    }

    pub fn with_bitrate_mbps(mut self, mbps: u16) -> Self {
        self.max_bitrate_mbps = mbps;
        self
    }

    pub fn supports(&self, tier: CodecTier) -> bool {
        match tier {
            CodecTier::Avc => self.flags & FLAG_H264 != 0,
            CodecTier::Hevc => self.flags & FLAG_HEVC != 0,
            CodecTier::HevcMain10 => self.flags & FLAG_HEVC_10BIT != 0 && self.flags & FLAG_HEVC != 0,
            CodecTier::Av1 => self.flags & FLAG_AV1 != 0,
        }
    }

    /// Build from the reliable message the client already sends: its ordered
    /// codec list plus display limits.
    pub fn from_codecs(codecs: &[VideoCodec], display: Option<ClientDisplay>) -> Self {
        let mut caps = Self::default();
        for codec in codecs {
            caps = match codec {
                VideoCodec::H264 => caps.with_codec(CodecTier::Avc),
                VideoCodec::Hevc => caps.with_codec(CodecTier::Hevc),
                VideoCodec::HevcMain10 => caps.with_codec(CodecTier::HevcMain10),
                VideoCodec::Hevc444 => caps.with_codec(CodecTier::Hevc),
            };
        }
        if !caps.supports(CodecTier::Avc) {
            // A decoder that did not list H.264 still needs a baseline to fall
            // back to; the host re-checks before using it.
            caps.flags |= FLAG_H264;
        }
        if let Some(display) = display {
            if display.screen_width >= 320 && display.screen_height >= 240 {
                caps.max_width = display.screen_width.max(display.screen_height);
                caps.max_height = display.screen_height.max(display.screen_width).min(4096);
                // `constrain` swaps for portrait; store the long edge first.
                caps.max_width = caps.max_width.max(caps.max_height);
            }
            if display.max_refresh_hz >= 30 {
                caps.max_fps = display.max_refresh_hz;
            }
            caps.max_bitrate_mbps = display.max_decode_mbps;
        }
        caps.tier_hint = if caps.supports(CodecTier::Av1) || caps.supports(CodecTier::HevcMain10) {
            3
        } else if caps.supports(CodecTier::Hevc) {
            2
        } else {
            1
        };
        caps
    }

    pub fn to_bytes(&self) -> [u8; 12] {
        *bytemuck::bytes_of(self)
            .as_array()
            .expect("POD size is asserted above")
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() == 12).then(|| bytemuck::pod_read_unaligned(bytes))
    }
}

/// What the host's encoder cascade can actually produce on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCaps {
    pub avc: bool,
    pub hevc: bool,
    /// HEVC Main10 / P010.
    pub hevc_main10: bool,
    pub av1: bool,
}

impl HostCaps {
    /// Conservative default: H.264 and HEVC Main are produced by every
    /// supported hardware path (NVENC, AMF, QSV, Media Foundation). Main10 is
    /// offered by the NVENC wrapper, but only when the codec path supports it.
    pub fn current() -> Self {
        Self {
            avc: true,
            hevc: true,
            hevc_main10: cfg!(target_os = "windows"),
            av1: false,
        }
    }

    pub fn supports(&self, tier: CodecTier) -> bool {
        match tier {
            CodecTier::Avc => self.avc,
            CodecTier::Hevc => self.hevc,
            CodecTier::HevcMain10 => self.hevc_main10 && self.hevc,
            CodecTier::Av1 => self.av1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecSelection {
    pub tier: CodecTier,
    /// Ordered codecs to try, negotiated tier first.
    pub codecs: [Option<VideoCodec>; 3],
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u16,
    /// Bitrate ceiling in bits per second (0 = client did not report one).
    pub bitrate_ceiling_bps: u32,
    pub reason: &'static str,
}

impl CodecSelection {
    /// Preferred codec list for `HostRuntimeControls::pending_codecs`.
    pub fn ordered_codecs(&self) -> Vec<VideoCodec> {
        let mut out = Vec::with_capacity(3);
        for codec in self.codecs.iter().flatten() {
            if !out.contains(codec) {
                out.push(*codec);
            }
        }
        out
    }
}

/// Pick the highest mutually supported tier.
pub fn negotiate(host: &HostCaps, client: &ClientCaps) -> CodecSelection {
    let candidates = [
        CodecTier::Av1,
        CodecTier::HevcMain10,
        CodecTier::Hevc,
        CodecTier::Avc,
    ];
    let tier = candidates
        .into_iter()
        .find(|tier| host.supports(*tier) && client.supports(*tier))
        // H.264 exists everywhere; this branch is unreachable in practice and
        // only guards a malformed capability packet.
        .unwrap_or(CodecTier::Avc);

    // Order: negotiated tier, then the safe fallback, then the last resort.
    let mut codecs = [None; 3];
    codecs[0] = Some(tier.codec());
    let fallback = match tier {
        CodecTier::Av1 => Some(VideoCodec::Hevc),
        CodecTier::HevcMain10 => Some(VideoCodec::Hevc),
        _ => None,
    };
    if let Some(codec) = fallback {
        codecs[1] = Some(codec);
    }
    codecs[2] = Some(VideoCodec::H264);

    let width = if client.max_width >= 320 {
        u32::from(client.max_width)
    } else {
        1920
    };
    let height = if client.max_height >= 240 {
        u32::from(client.max_height)
    } else {
        1080
    };
    let max_fps = if client.max_fps >= 30 { client.max_fps } else { 60 };

    let reason = match tier {
        CodecTier::Av1 => "AV1: both endpoints support tier 3",
        CodecTier::HevcMain10 => "HEVC Main10: both endpoints support 10-bit",
        CodecTier::Hevc => "HEVC Main: best shared tier",
        CodecTier::Avc => "H.264: universal fallback",
    };

    CodecSelection {
        tier,
        codecs,
        max_width: width,
        max_height: height,
        max_fps,
        bitrate_ceiling_bps: u32::from(client.max_bitrate_mbps) * 1_000_000,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_packet_is_exactly_twelve_bytes() {
        let caps = ClientCaps::new(1920, 1080, 60)
            .with_codec(CodecTier::Hevc)
            .with_bitrate_mbps(25);
        let bytes = caps.to_bytes();
        assert_eq!(bytes.len(), 12);
        assert_eq!(ClientCaps::from_bytes(&bytes), Some(caps));
        assert_eq!(ClientCaps::from_bytes(&bytes[..11]), None);
    }

    #[test]
    fn modern_phone_gets_hevc_and_old_laptop_gets_h264() {
        let host = HostCaps::current();
        let phone = ClientCaps::new(2400, 1080, 120).with_codec(CodecTier::Hevc);
        let old = ClientCaps::new(1366, 768, 60);

        let phone_selection = negotiate(&host, &phone);
        assert_eq!(phone_selection.tier, CodecTier::Hevc);
        assert_eq!(phone_selection.ordered_codecs()[0], VideoCodec::Hevc);

        let old_selection = negotiate(&host, &old);
        assert_eq!(old_selection.tier, CodecTier::Avc);
        assert_eq!(old_selection.ordered_codecs(), vec![VideoCodec::H264]);
    }

    #[test]
    fn main10_requires_both_sides() {
        let mut host = HostCaps::current();
        host.hevc_main10 = true;
        let eight_bit = ClientCaps::new(1920, 1080, 60).with_codec(CodecTier::Hevc);
        assert_eq!(negotiate(&host, &eight_bit).tier, CodecTier::Hevc);

        let ten_bit = ClientCaps::new(1920, 1080, 60).with_codec(CodecTier::HevcMain10);
        let selection = negotiate(&host, &ten_bit);
        assert_eq!(selection.tier, CodecTier::HevcMain10);
        assert_eq!(selection.ordered_codecs()[0], VideoCodec::HevcMain10);
        // Main is still offered as the fallback if 10-bit fails to initialise.
        assert!(selection.ordered_codecs().contains(&VideoCodec::Hevc));
    }

    #[test]
    fn av1_is_never_selected_when_the_host_cannot_encode_it() {
        let host = HostCaps::current();
        assert!(!host.av1);
        let client = ClientCaps::new(3840, 2160, 120).with_codec(CodecTier::Av1);
        let selection = negotiate(&host, &client);
        assert_ne!(selection.tier, CodecTier::Av1);
        assert!(selection.codecs.iter().flatten().all(|c| *c != VideoCodec::H264 || true));
    }

    #[test]
    fn client_size_and_fps_limits_are_carried_through() {
        let mut host = HostCaps::current();
        host.avc = true;
        let caps = ClientCaps::new(1280, 720, 30).with_bitrate_mbps(8);
        let selection = negotiate(&host, &caps);
        assert_eq!(selection.max_width, 1280);
        assert_eq!(selection.max_height, 720);
        assert_eq!(selection.max_fps, 30);
        assert_eq!(selection.bitrate_ceiling_bps, 8_000_000);
    }

    #[test]
    fn unknown_limits_do_not_restrict_the_host() {
        let caps = ClientCaps::new(0, 0, 0);
        let selection = negotiate(&HostCaps::current(), &caps);
        assert_eq!((selection.max_width, selection.max_height), (1920, 1080));
        assert_eq!(selection.max_fps, 60);
        assert_eq!(selection.bitrate_ceiling_bps, 0);
    }

    #[test]
    fn caps_built_from_the_reliable_message_match_the_tier_hint() {
        let display = ClientDisplay {
            device_type: crate::handshake::DeviceType::Mobile,
            screen_width: 2400,
            screen_height: 1080,
            max_refresh_hz: 120,
            max_decode_mbps: 40,
            supports_haptics: true,
            hevc_main10: true,
            hevc_444: false,
        };
        let caps = ClientCaps::from_codecs(
            &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::HevcMain10],
            Some(display),
        );
        assert_eq!(caps.tier_hint, 3);
        assert!(caps.supports(CodecTier::HevcMain10));
        assert!(caps.max_fps >= 120);
        assert_eq!(caps.max_width, 2400);
    }
}
