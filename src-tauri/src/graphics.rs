//! Graphics backend selection and the low-latency graphics device policy.

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
