//! Graphics backend selection, low-latency device policy, and client-side
//! BT.709 Full-Range + FidelityFX CAS sharpening shaders.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Pick the best available: D3D12 when present, else D3D11.
    Auto,
    D3D11,
    D3D12,
    Vulkan,
}

impl Backend {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "d3d11" => Self::D3D11,
            "d3d12" => Self::D3D12,
            "vulkan" => Self::Vulkan,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::D3D11 => "D3D11",
            Self::D3D12 => "D3D12",
            Self::Vulkan => "Vulkan",
        }
    }
}

/// What this machine can actually create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendCaps {
    pub d3d11: bool,
    pub d3d12: bool,
    pub vulkan: bool,
    /// Backend the capture path will use for the requested setting.
    pub effective: &'static str,
    /// Why the effective backend differs from the requested one.
    pub note: &'static str,
}

/// Client-side HLSL Pixel Shader for BT.709 Full-Range (0–255) NV12 -> RGB conversion
/// combined with a single-pass AMD FidelityFX Contrast Adaptive Sharpening (CAS) filter.
pub const CAS_BT709_FULL_RANGE_HLSL: &str = r#"
Texture2D<float>  tex_y  : register(t0);
Texture2D<float2> tex_uv : register(t1);
SamplerState      samp   : register(s0);

cbuffer CasParams : register(b0) {
    float2 inv_tex_size;
    float  sharpness; // 0.0 to 1.0 (default 0.45)
    float  full_range; // 1.0 = Full Range (0-255), 0.0 = Studio (16-235)
};

float3 sample_bt709_rgb(float2 uv) {
    float y_raw  = tex_y.SampleLevel(samp, uv, 0);
    float2 cbcr  = tex_uv.SampleLevel(samp, uv, 0) - float2(0.5, 0.5);
    float y = (full_range > 0.5) ? y_raw : saturate((y_raw - (16.0 / 255.0)) * (255.0 / 219.0));
    float2 c = (full_range > 0.5) ? cbcr : cbcr * (255.0 / 224.0);

    // Exact BT.709 Full Range matrix
    float r = y + 1.5748 * c.y;
    float g = y - 0.1873 * c.x - 0.4681 * c.y;
    float b = y + 1.8556 * c.x;
    return saturate(float3(r, g, b));
}

float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 c = sample_bt709_rgb(uv);
    if (sharpness <= 0.01) {
        return float4(c, 1.0);
    }
    float3 n = sample_bt709_rgb(uv + float2(0.0, -inv_tex_size.y));
    float3 s = sample_bt709_rgb(uv + float2(0.0,  inv_tex_size.y));
    float3 w = sample_bt709_rgb(uv + float2(-inv_tex_size.x, 0.0));
    float3 e = sample_bt709_rgb(uv + float2( inv_tex_size.x, 0.0));

    float3 min_rgb = min(c, min(min(n, s), min(w, e)));
    float3 max_rgb = max(c, max(max(n, s), max(w, e)));
    float3 amp = saturate(min(min_rgb, 1.0 - max_rgb) / max(max_rgb, 1e-4));
    float3 weight = -sqrt(amp) * lerp(0.125, 0.20, saturate(sharpness));

    float3 out_rgb = saturate((c + (n + s + w + e) * weight) / (1.0 + 4.0 * weight));
    return float4(out_rgb, 1.0);
}
"#;

#[cfg(target_os = "windows")]
fn has_export(dll: &str, symbol: &str) -> bool {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{PCSTR, PCWSTR};

    let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
    let Ok(name) = std::ffi::CString::new(symbol) else {
        return false;
    };
    unsafe {
        let Ok(module) = LoadLibraryW(PCWSTR(wide.as_ptr())) else {
            return false;
        };
        GetProcAddress(module, PCSTR(name.as_ptr() as *const u8)).is_some()
    }
}

#[cfg(not(target_os = "windows"))]
fn has_export(_dll: &str, _symbol: &str) -> bool {
    false
}

/// Probe the process for available graphics APIs. D3D11 is always reported on
/// Windows because the capture path links against it directly.
pub fn probe() -> BackendCaps {
    #[cfg(target_os = "windows")]
    {
        BackendCaps {
            d3d11: true,
            d3d12: has_export("d3d12.dll", "D3D12CreateDevice"),
            vulkan: has_export("vulkan-1.dll", "vkCreateInstance"),
            effective: "D3D11",
            note: "DXGI Desktop Duplication requires D3D11 zero-copy encode",
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        BackendCaps {
            d3d11: false,
            d3d12: false,
            vulkan: has_export("libvulkan.so.1", "vkCreateInstance"),
            effective: "Vulkan",
            note: "hardware capture is Windows-only; content is decoded here",
        }
    }
}

/// Resolve a requested backend to the one the capture path will use.
pub fn effective(requested: Backend) -> (Backend, &'static str) {
    let caps = probe();
    match requested {
        Backend::Auto => (Backend::D3D11, "auto: lowest-latency zero-copy path"),
        Backend::D3D11 => (Backend::D3D11, "D3D11 approved"),
        Backend::D3D12 if caps.d3d12 => (
            Backend::D3D12,
            "D3D12 detected; capture still uses D3D11 duplication",
        ),
        Backend::Vulkan if caps.vulkan => (
            Backend::Vulkan,
            "Vulkan detected; capture still uses D3D11 duplication",
        ),
        Backend::D3D12 => (Backend::D3D11, "D3D12 unavailable on this machine"),
        Backend::Vulkan => (Backend::D3D11, "Vulkan loader unavailable"),
    }
}

/// One-frame-deep frame latency on the DXGI device. Must be called once the
/// device exists; a failure is non-fatal on old drivers.
#[cfg(target_os = "windows")]
pub fn apply_d3d11_latency_policy(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) {
    use windows::Win32::Graphics::Dxgi::IDXGIDevice1;
    use windows::core::Interface;
    if let Ok(dxgi) = device.cast::<IDXGIDevice1>() {
        let _ = unsafe { dxgi.SetMaximumFrameLatency(1) };
    }
}

#[cfg(not(target_os = "windows"))]
pub fn apply_d3d11_latency_policy<T>(_device: &T) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parsing_is_forgiving() {
        assert_eq!(Backend::parse("D3D12"), Backend::D3D12);
        assert_eq!(Backend::parse(" vulkan "), Backend::Vulkan);
        assert_eq!(Backend::parse("d3d11"), Backend::D3D11);
        assert_eq!(Backend::parse("garbage"), Backend::Auto);
        assert_eq!(Backend::Auto.label(), "Auto");
    }

    #[test]
    fn probe_never_reports_an_impossible_effective_backend() {
        let caps = probe();
        let (backend, note) = effective(Backend::Auto);
        assert!(!note.is_empty());
        if !caps.d3d12 {
            assert_eq!(effective(Backend::D3D12).0, Backend::D3D11);
        }
        if !caps.vulkan {
            assert_eq!(effective(Backend::Vulkan).0, Backend::D3D11);
        }
        assert!(matches!(
            backend,
            Backend::D3D11 | Backend::D3D12 | Backend::Vulkan
        ));
    }

    #[test]
    fn caps_serialize_for_the_ui() {
        let json = serde_json::to_value(probe()).expect("serialize");
        for key in ["d3d11", "d3d12", "vulkan", "effective", "note"] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}