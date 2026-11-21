//! Windows native video capability and playback diagnostics.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SurfaceRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Deserialize)]
pub struct SurfaceLayout {
    pub picture: SurfaceRect,
    #[cfg(target_os = "windows")]
    pub controls: Vec<SurfaceRect>,
    pub visible: bool,
}

pub const SWAP_EFFECT: &str = "DXGI_SWAP_EFFECT_FLIP_DISCARD";
pub const PRESENT_SYNC_INTERVAL: u32 = 0;
pub const MAX_FRAME_LATENCY: u32 = 1;
pub const FRAME_LATENCY_WAITABLE_OBJECT: bool = true;
pub const HIDDEN_STREAM_CURSOR: bool = true;
pub const LOCAL_CURSOR_OVERLAY: bool = true;
pub const VIDEO_DECODE_SURFACE: &str = "NV12 ID3D11Texture2D";
pub const DECODER_API: &str = "Media Foundation DXVA";

/// Shader inputs used by the native one-pass presentation pipeline.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeD3D11Policy {
    pub swap_effect: &'static str,
    pub present_sync_interval: u32,
    pub max_frame_latency: u32,
    pub frame_latency_waitable_object: bool,
    pub decode_surface: &'static str,
    pub decoder_api: &'static str,
    pub hidden_stream_cursor: bool,
    pub local_cursor_overlay: bool,
}

pub const POLICY: NativeD3D11Policy = NativeD3D11Policy {
    swap_effect: SWAP_EFFECT,
    present_sync_interval: PRESENT_SYNC_INTERVAL,
    max_frame_latency: MAX_FRAME_LATENCY,
    frame_latency_waitable_object: FRAME_LATENCY_WAITABLE_OBJECT,
    decode_surface: VIDEO_DECODE_SURFACE,
    decoder_api: DECODER_API,
    hidden_stream_cursor: HIDDEN_STREAM_CURSOR,
    local_cursor_overlay: LOCAL_CURSOR_OVERLAY,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeD3D11BackendState {
    pub available: bool,
    pub active: bool,
    pub policy: NativeD3D11Policy,
    pub reason: &'static str,
}

/// Reports device capability and whether native playback is active.
pub fn state(_active: bool) -> NativeD3D11BackendState {
    #[cfg(target_os = "windows")]
    {
        let caps = crate::graphics::probe();
        NativeD3D11BackendState {
            available: caps.d3d11,
            active: _active,
            policy: POLICY,
            reason: if _active {
                "Presenting hardware-decoded video on D3D11"
            } else {
                "Waiting for a D3D11 video surface and a supported decoder"
            },
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        NativeD3D11BackendState {
            available: false,
            active: false,
            policy: POLICY,
            reason: "native D3D11 presentation is Windows-only",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_single_frame_and_immediate() {
        assert_eq!(POLICY.swap_effect, "DXGI_SWAP_EFFECT_FLIP_DISCARD");
        assert_eq!(POLICY.present_sync_interval, 0);
        assert_eq!(POLICY.max_frame_latency, 1);
        assert!(POLICY.frame_latency_waitable_object);
        assert_eq!(POLICY.decode_surface, "NV12 ID3D11Texture2D");
    }
}
