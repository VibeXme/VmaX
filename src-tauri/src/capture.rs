use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Global lock-free Parsec-style telemetry accumulators (microseconds & sliding 1-sec bitrate)
static SMOOTH_ENCODE_US: AtomicU32 = AtomicU32::new(0);
static SMOOTH_DECODE_US: AtomicU32 = AtomicU32::new(0);
static SMOOTH_NETWORK_US: AtomicU32 = AtomicU32::new(0);
static CONSTANT_FPS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Updates smoothed host encode latency (`Encode 2.45ms`) using a 15% EMA filter.
pub fn record_encode_latency_us(encode_us: u32) -> f32 {
    let prev = SMOOTH_ENCODE_US.load(Ordering::Relaxed);
    let next = if prev == 0 {
        encode_us
    } else {
        ((prev as u64 * 85 + encode_us as u64 * 15) / 100) as u32
    };
    SMOOTH_ENCODE_US.store(next, Ordering::Relaxed);
    next as f32 / 1000.0
}

/// Updates smoothed client hardware decode latency (`Decode 5.50ms`) using a 15% EMA filter.
pub fn record_decode_latency_us(decode_us: u32) -> f32 {
    let prev = SMOOTH_DECODE_US.load(Ordering::Relaxed);
    let next = if prev == 0 {
        decode_us
    } else {
        ((prev as u64 * 85 + decode_us as u64 * 15) / 100) as u32
    };
    SMOOTH_DECODE_US.store(next, Ordering::Relaxed);
    next as f32 / 1000.0
}

/// Updates smoothed one-way network latency (`Network = RTT / 2`) from QUIC ping/pong RTT in microseconds.
pub fn record_network_rtt_us(rtt_us: u32) -> f32 {
    let one_way_us = rtt_us / 2;
    let prev = SMOOTH_NETWORK_US.load(Ordering::Relaxed);
    let next = if prev == 0 {
        one_way_us
    } else {
        ((prev as u64 * 85 + one_way_us as u64 * 15) / 100) as u32
    };
    SMOOTH_NETWORK_US.store(next, Ordering::Relaxed);
    next as f32 / 1000.0
}

pub fn reset_client_decode_latency() {
    SMOOTH_DECODE_US.store(0, Ordering::Relaxed);
}

pub fn latest_encode_latency_ms() -> f32 {
    SMOOTH_ENCODE_US.load(Ordering::Relaxed) as f32 / 1000.0
}

pub fn latest_decode_latency_ms() -> f32 {
    SMOOTH_DECODE_US.load(Ordering::Relaxed) as f32 / 1000.0
}

pub fn latest_network_latency_ms() -> f32 {
    SMOOTH_NETWORK_US.load(Ordering::Relaxed) as f32 / 1000.0
}

/// When set, the encoded frame is always the host's configured size. Bitrate
/// pressure is absorbed by rate control and QP instead of by downscaling.
static LOCK_RESOLUTION: AtomicBool = AtomicBool::new(true);

pub fn set_lock_resolution(enabled: bool) {
    LOCK_RESOLUTION.store(enabled, Ordering::Relaxed);
}

pub fn is_lock_resolution() -> bool {
    LOCK_RESOLUTION.load(Ordering::Relaxed)
}

pub fn set_constant_fps(enabled: bool) {
    CONSTANT_FPS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_constant_fps() -> bool {
    CONSTANT_FPS_ENABLED.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareTelemetryPayload {
    pub nvenc_load_pct: f32,
    pub vram_used_gb: f32,
    pub vram_total_gb: f32,
    pub cpu_load_pct: f32,
    pub encode_latency_ms: f32,
    pub decode_latency_ms: f32,
    pub fps: u32,
    pub pipeline_label: String,
}

impl Default for HardwareTelemetryPayload {
    fn default() -> Self {
        Self {
            nvenc_load_pct: 0.0,
            vram_used_gb: 0.0,
            vram_total_gb: 0.0,
            cpu_load_pct: 0.0,
            encode_latency_ms: latest_encode_latency_ms(),
            decode_latency_ms: latest_decode_latency_ms(),
            fps: 0,
            pipeline_label: "Standby".to_string(),
        }
    }
}

/// Sub-millisecond frame pacer for uncapped 60 / 120 / 144 FPS streaming.
/// Avoids Windows' default 15.6 ms sleep granularity by yielding only for the final ~1.5 ms.
pub struct HighResFramePacer {
    next_tick: Instant,
}

impl Default for HighResFramePacer {
    fn default() -> Self {
        Self {
            next_tick: Instant::now(),
        }
    }
}

impl HighResFramePacer {
    pub fn wait_next_frame(&mut self, target_fps: u16) {
        let interval = Duration::from_secs_f64(1.0 / f64::from(target_fps.max(1)));
        self.next_tick += interval;
        let now = Instant::now();
        if self.next_tick <= now {
            if now.duration_since(self.next_tick) > interval * 2 {
                self.next_tick = now;
            }
            return;
        }
        let remaining = self.next_tick.duration_since(now);
        if remaining > Duration::from_millis(2) {
            std::thread::sleep(remaining - Duration::from_millis(2));
        }
        while Instant::now() < self.next_tick {
            std::hint::spin_loop();
        }
    }
}

pub fn parse_resolution_ceiling(label: &str) -> (u32, u32) {
    match label.trim().to_ascii_lowercase().as_str() {
        "720p" => (1280, 720),
        "1080p" => (1920, 1080),
        "1440p" => (2560, 1440),
        "4k" | "2160p" => (3840, 2160),
        _ => (1920, 1080),
    }
}

/// Scales `(native_w, native_h)` to fit inside `(max_w, max_h)` while preserving aspect ratio,
/// never upscaling beyond native resolution, and aligning dimensions to even pixels (`>= 64`).
pub fn fit_resolution(native_w: u32, native_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let nw = native_w.max(64);
    let nh = native_h.max(64);
    let mw = max_w.max(64);
    let mh = max_h.max(64);

    if nw <= mw && nh <= mh {
        return ((nw & !1).max(64), (nh & !1).max(64));
    }

    let scale_w = mw as f64 / nw as f64;
    let scale_h = mh as f64 / nh as f64;
    let scale = scale_w.min(scale_h).min(1.0);

    let out_w = ((nw as f64 * scale).floor() as u32 & !1).max(64);
    let out_h = ((nh as f64 * scale).floor() as u32 & !1).max(64);
    (out_w, out_h)
}

#[derive(Debug, Clone, Copy)]
pub struct CpuTimeSample {
    idle: u64,
    kernel: u64,
    user: u64,
}

/// Computes OS-wide CPU utilization percentage across cores since `prev`.
pub fn sample_cpu_load_pct(prev: &mut Option<CpuTimeSample>) -> f32 {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetSystemTimes;

        fn ft_to_u64(ft: FILETIME) -> u64 {
            ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
        }

        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok =
            unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_ok();
        if !ok {
            return 0.0;
        }
        let cur = CpuTimeSample {
            idle: ft_to_u64(idle),
            kernel: ft_to_u64(kernel),
            user: ft_to_u64(user),
        };
        let load = if let Some(p) = *prev {
            let d_idle = cur.idle.saturating_sub(p.idle);
            let d_kernel = cur.kernel.saturating_sub(p.kernel);
            let d_user = cur.user.saturating_sub(p.user);
            let total = d_kernel.saturating_add(d_user);
            if total > 0 {
                let busy = total.saturating_sub(d_idle);
                ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
            } else {
                0.0
            }
        } else {
            0.0
        };
        *prev = Some(cur);
        load
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = prev;
        0.0
    }
}

#[cfg(target_os = "windows")]
pub mod win {
    use super::{fit_resolution, is_constant_fps};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
        D3D11_VIDEO_PROCESSOR_FILTER, D3D11_VIDEO_PROCESSOR_FILTER_RANGE,
        D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
        D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
        ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
        ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
        ID3D11VideoProcessorOutputView,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_MODE_ROTATION,
        DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
        DXGI_MODE_ROTATION_ROTATE270, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1,
        IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
    };
    use windows_core::{BOOL, Interface};

    pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;

    pub(crate) struct VideoScaler {
        video_device: ID3D11VideoDevice,
        video_context: ID3D11VideoContext,
        enumerator: ID3D11VideoProcessorEnumerator,
        processor: ID3D11VideoProcessor,
        output_view: ID3D11VideoProcessorOutputView,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    }

    impl VideoScaler {
        pub(crate) fn new(
            device: &ID3D11Device,
            context: &ID3D11DeviceContext,
            work_tex: &ID3D11Texture2D,
            src_w: u32,
            src_h: u32,
            dst_w: u32,
            dst_h: u32,
        ) -> Result<Self, String> {
            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| format!("ID3D11VideoDevice unavailable: {e}"))?;
            let video_context: ID3D11VideoContext = context
                .cast()
                .map_err(|e| format!("ID3D11VideoContext unavailable: {e}"))?;

            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 144,
                    Denominator: 1,
                },
                InputWidth: src_w,
                InputHeight: src_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 144,
                    Denominator: 1,
                },
                OutputWidth: dst_w,
                OutputHeight: dst_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };

            let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
                .map_err(|e| format!("CreateVideoProcessorEnumerator failed: {e}"))?;
            let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
                .map_err(|e| format!("CreateVideoProcessor failed: {e}"))?;

            let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: Default::default(),
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            let work_res: ID3D11Resource = work_tex
                .cast()
                .map_err(|e| format!("work_tex cast failed: {e}"))?;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &work_res,
                    &enumerator,
                    &out_view_desc,
                    Some(&mut output_view),
                )
            }
            .map_err(|e| format!("CreateVideoProcessorOutputView failed: {e}"))?;

            let output_view =
                output_view.ok_or_else(|| "null VideoProcessorOutputView".to_string())?;

            unsafe {
                let src_rect = RECT {
                    left: 0,
                    top: 0,
                    right: src_w as i32,
                    bottom: src_h as i32,
                };
                let dst_rect = RECT {
                    left: 0,
                    top: 0,
                    right: dst_w as i32,
                    bottom: dst_h as i32,
                };
                video_context.VideoProcessorSetStreamSourceRect(
                    &processor,
                    0,
                    true,
                    Some(&src_rect),
                );
                video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst_rect));
                video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

                let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
                work_tex.GetDesc(&mut texture_desc);
                if let Ok(context1) = video_context.cast::<ID3D11VideoContext1>() {
                    context1.VideoProcessorSetStreamColorSpace1(
                        &processor,
                        0,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                    if matches!(texture_desc.Format, DXGI_FORMAT_NV12 | DXGI_FORMAT_P010) {
                        // Use Full-Range BT.709 YCbCr (0-1023) instead of Studio Limited (64-940)
                        context1.VideoProcessorSetOutputColorSpace1(
                            &processor,
                            DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                        );
                    } else {
                        context1.VideoProcessorSetOutputColorSpace1(
                            &processor,
                            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                        );
                    }
                } else if matches!(texture_desc.Format, DXGI_FORMAT_NV12 | DXGI_FORMAT_P010) {
                    return Err("Full-range YUV conversion requires ID3D11VideoContext1".into());
                }
            }

            let mut scaler = Self {
                video_device,
                video_context,
                enumerator,
                processor,
                output_view,
                src_w,
                src_h,
                dst_w,
                dst_h,
            };
            if dst_w != src_w || dst_h != src_h {
                scaler.set_sharpness(35);
            }
            Ok(scaler)
        }

        pub(crate) fn set_sharpness(&mut self, level: u8) {
            // D3D11_VIDEO_PROCESSOR_FILTER_EDGE_ENHANCEMENT. This used to pass
            // filter index 2, which is HUE: the scaler shifted colours instead
            // of sharpening, and the intended edge enhancement never ran.
            let filter = D3D11_VIDEO_PROCESSOR_FILTER(5);
            if level == 0 {
                let _ = unsafe {
                    self.video_context.VideoProcessorSetStreamFilter(
                        &self.processor,
                        0,
                        filter,
                        false,
                        0,
                    )
                };
                return;
            }
            let Ok(range): Result<D3D11_VIDEO_PROCESSOR_FILTER_RANGE, _> =
                (unsafe { self.enumerator.GetVideoProcessorFilterRange(filter) })
            else {
                return;
            };
            let span = range.Maximum - range.Minimum;
            let target = range.Minimum + span * i32::from(level.min(100)) / 100;
            let _ = unsafe {
                self.video_context.VideoProcessorSetStreamFilter(
                    &self.processor,
                    0,
                    filter,
                    true,
                    target.clamp(range.Minimum, range.Maximum),
                )
            };
        }

        pub(crate) fn scale(&self, src_tex: &ID3D11Texture2D) -> Result<(), String> {
            let src_res: ID3D11Resource = src_tex
                .cast()
                .map_err(|e| format!("src_tex cast failed: {e}"))?;
            let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: Default::default(),
                },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    &src_res,
                    &self.enumerator,
                    &in_view_desc,
                    Some(&mut in_view),
                )
            }
            .map_err(|e| format!("CreateVideoProcessorInputView failed: {e}"))?;

            let Some(in_view) = in_view else {
                return Err("null VideoProcessorInputView".to_string());
            };

            let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: core::mem::ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            }];
            let res = unsafe {
                self.video_context.VideoProcessorBlt(
                    &self.processor,
                    &self.output_view,
                    0,
                    &streams,
                )
            };
            let [stream] = streams;
            let _ = core::mem::ManuallyDrop::into_inner(stream.pInputSurface);
            let _ = (self.src_w, self.src_h, self.dst_w, self.dst_h);
            res.map_err(|e| format!("VideoProcessorBlt failed: {e}"))
        }
    }

    struct FrameReleaseGuard<'a>(&'a IDXGIOutputDuplication);

    impl Drop for FrameReleaseGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                let _ = self.0.ReleaseFrame();
            }
        }
    }

    pub struct DxgiCapturer {
        pub device: ID3D11Device,
        pub context: ID3D11DeviceContext,
        duplication: Option<IDXGIOutputDuplication>,
        output1: Option<IDXGIOutput1>,
        rotation: DXGI_MODE_ROTATION,
        work_tex: ID3D11Texture2D,
        staging_tex: Option<ID3D11Texture2D>,
        scale_staging: Option<(u32, u32, ID3D11Texture2D)>,
        scale_pixels: Vec<u8>,
        scaler: Option<VideoScaler>,
        pub native_width: u32,
        pub native_height: u32,
        pub desktop_bounds: windows::Win32::Foundation::RECT,
        pub encode_width: u32,
        pub encode_height: u32,
        pub adapter_vendor_id: u32,
        pub adapter_luid: i64,
        pub vram_total_gb: f32,
        pub adapter_name: String,
        has_valid_frame: bool,
        surface_size: (u32, u32),
        /// The desktop has stopped changing since the last captured frame.
        static_screen: bool,
        /// Set once per motion -> static transition. The capture loop drains it
        /// and asks the encoder for two near-lossless refinement frames.
        refinement_requested: bool,
        /// Remaining progressive refinement frames after desktop motion stops
        refinement_frames_left: u8,
        /// Mouse pointer bitmap, captured separately by Desktop Duplication.
        pointer_shape: Option<crate::cursor::PointerShape>,
        /// Pointer position in desktop (native) pixels.
        pointer_position: (i32, i32),
        pointer_visible: bool,
        /// Whether the pointer is drawn into the streamed frame. Off means the
        /// client draws its own pointer from local input, which is lower
        /// latency but cannot follow host-side state.
        pointer_overlay: bool,
    }

    unsafe impl Send for DxgiCapturer {}

    fn luid_to_i64(low: u32, high: i32) -> i64 {
        ((high as i64) << 32) | (low as u64 as i64)
    }

    fn create_d3d11_for_adapter(
        adapter: &IDXGIAdapter1,
    ) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL::default();

        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        }
        .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;

        let device = device.ok_or_else(|| "D3D11CreateDevice returned null device".to_string())?;
        crate::graphics::apply_d3d11_latency_policy(&device);
        let context =
            context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;

        if let Ok(dxgi_dev) = device.cast::<IDXGIDevice>() {
            unsafe {
                let _ = dxgi_dev.SetGPUThreadPriority(7);
            }
        }

        Ok((device, context))
    }

    pub fn create_bgra_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        staging: bool,
    ) -> Result<ID3D11Texture2D, String> {
        create_capture_texture(device, width, height, staging, DXGI_FORMAT_B8G8R8A8_UNORM)
    }

    pub(crate) fn create_capture_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        staging: bool,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11Texture2D, String> {
        let bind_flags = if staging {
            0
        } else {
            (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32
        };
        create_texture(device, width, height, format, staging, bind_flags)
    }

    pub(crate) fn create_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        staging: bool,
        bind_flags: u32,
    ) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: bind_flags,
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| format!("CreateTexture2D failed: {e}"))?;
        tex.ok_or_else(|| "CreateTexture2D returned null texture".to_string())
    }

    pub fn open_nvidia_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext, f32), String> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| format!("CreateDXGIFactory1 failed: {e}"))?;
        let mut idx = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(idx) } {
            idx += 1;
            let desc =
                unsafe { adapter.GetDesc1() }.map_err(|e| format!("GetDesc1 failed: {e}"))?;
            if desc.VendorId == NVIDIA_VENDOR_ID {
                let (dev, ctx) = create_d3d11_for_adapter(&adapter)?;
                let vram_gb =
                    (desc.DedicatedVideoMemory as f64 / (1024.0 * 1024.0 * 1024.0)) as f32;
                return Ok((dev, ctx, vram_gb));
            }
        }
        Err("no NVIDIA DXGI adapter found".to_string())
    }

    #[derive(serde::Serialize)]
    pub struct CaptureDisplay {
        id: String,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
    }

    fn display_id(desc: &DXGI_OUTPUT_DESC) -> String {
        let end = desc
            .DeviceName
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(desc.DeviceName.len());
        String::from_utf16_lossy(&desc.DeviceName[..end])
    }

    pub fn list_displays() -> Result<Vec<CaptureDisplay>, String> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| e.to_string())?;
        let mut displays = Vec::new();
        for adapter_index in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
                break;
            };
            for output_index in 0.. {
                let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                    break;
                };
                let Ok(desc) = (unsafe { output.GetDesc() }) else {
                    continue;
                };
                if desc.AttachedToDesktop.as_bool() {
                    let rect = desc.DesktopCoordinates;
                    displays.push(CaptureDisplay {
                        id: display_id(&desc),
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                        x: rect.left,
                        y: rect.top,
                    });
                }
            }
        }
        displays.sort_by_key(|display| (display.x != 0 || display.y != 0, display.x, display.y));
        Ok(displays)
    }

    impl DxgiCapturer {
        pub fn new(max_width: u32, max_height: u32, display: Option<&str>) -> Result<Self, String> {
            unsafe {
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }

            let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
                .map_err(|e| format!("CreateDXGIFactory1 failed: {e}"))?;

            let mut best_vram_bytes: usize = 0;
            let mut chosen: Option<(
                ID3D11Device,
                ID3D11DeviceContext,
                IDXGIOutput1,
                IDXGIOutputDuplication,
                DXGI_ADAPTER_DESC1,
                u32,
                u32,
                windows::Win32::Foundation::RECT,
            )> = None;

            let mut adapter_idx = 0u32;
            while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_idx) } {
                adapter_idx += 1;
                let Ok(adapter_desc) = (unsafe { adapter.GetDesc1() }) else {
                    continue;
                };
                if adapter_desc.DedicatedVideoMemory > best_vram_bytes {
                    best_vram_bytes = adapter_desc.DedicatedVideoMemory;
                }

                if chosen.is_some() {
                    continue;
                }

                let mut out_idx = 0u32;
                while let Ok(output) = unsafe { adapter.EnumOutputs(out_idx) } {
                    out_idx += 1;
                    let Ok(out_desc): Result<DXGI_OUTPUT_DESC, _> = (unsafe { output.GetDesc() })
                    else {
                        continue;
                    };
                    if !out_desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    if display.is_some_and(|id| id != display_id(&out_desc)) {
                        continue;
                    }
                    let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                        continue;
                    };
                    let Ok((device, context)) = create_d3d11_for_adapter(&adapter) else {
                        continue;
                    };
                    match unsafe { output1.DuplicateOutput(&device) } {
                        Ok(dupl) => {
                            let desc = unsafe { dupl.GetDesc() };
                            let mode = desc.ModeDesc;
                            let rotated = matches!(
                                desc.Rotation,
                                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
                            );
                            let w = if !rotated && mode.Width >= 64 {
                                mode.Width
                            } else {
                                (out_desc.DesktopCoordinates.right
                                    - out_desc.DesktopCoordinates.left)
                                    .max(64) as u32
                            };
                            let h = if !rotated && mode.Height >= 64 {
                                mode.Height
                            } else {
                                (out_desc.DesktopCoordinates.bottom
                                    - out_desc.DesktopCoordinates.top)
                                    .max(64) as u32
                            };
                            chosen = Some((
                                device,
                                context,
                                output1,
                                dupl,
                                adapter_desc,
                                w,
                                h,
                                out_desc.DesktopCoordinates,
                            ));
                            break;
                        }
                        Err(_) => continue,
                    }
                }
            }

            let Some((
                device,
                context,
                output1,
                dupl,
                adapter_desc,
                native_w,
                native_h,
                desktop_bounds,
            )) = chosen
            else {
                return Err(match display {
                    Some(id) => format!("Could not capture selected display {id}"),
                    None => {
                        "No desktop-attached display could initialize Desktop Duplication".into()
                    }
                });
            };

            let (encode_w, encode_h) = fit_resolution(native_w, native_h, max_width, max_height);
            let rotation = unsafe { dupl.GetDesc() }.Rotation;
            let work_tex = create_bgra_texture(&device, encode_w, encode_h, false)?;
            let scaler = if encode_w != native_w || encode_h != native_h {
                VideoScaler::new(
                    &device, &context, &work_tex, native_w, native_h, encode_w, encode_h,
                )
                .ok()
            } else {
                None
            };

            let name_len = adapter_desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(adapter_desc.Description.len());
            let adapter_name = String::from_utf16_lossy(&adapter_desc.Description[..name_len]);
            let vram_total_gb = (best_vram_bytes.max(adapter_desc.DedicatedVideoMemory) as f64
                / (1024.0 * 1024.0 * 1024.0)) as f32;

            Ok(Self {
                device,
                context,
                duplication: Some(dupl),
                output1: Some(output1),
                rotation,
                work_tex,
                staging_tex: None,
                scale_staging: None,
                scale_pixels: Vec::new(),
                scaler,
                native_width: native_w,
                native_height: native_h,
                desktop_bounds,
                encode_width: encode_w,
                encode_height: encode_h,
                adapter_vendor_id: adapter_desc.VendorId,
                adapter_luid: luid_to_i64(
                    adapter_desc.AdapterLuid.LowPart,
                    adapter_desc.AdapterLuid.HighPart,
                ),
                vram_total_gb,
                adapter_name,
                has_valid_frame: false,
                surface_size: (0, 0),
                static_screen: false,
                refinement_requested: false,
                refinement_frames_left: 2,
                pointer_shape: None,
                pointer_position: (0, 0),
                pointer_visible: false,
                pointer_overlay: true,
            })
        }

        pub fn texture(&self) -> &ID3D11Texture2D {
            &self.work_tex
        }

        /// True while the desktop is static and the host is thinning the stream.
        pub fn is_static_screen(&self) -> bool {
            self.static_screen
        }

        /// True exactly once per motion -> static transition.
        pub fn take_refinement_request(&mut self) -> bool {
            core::mem::replace(&mut self.refinement_requested, false)
        }

        pub fn needs_recovery(&self) -> bool {
            unsafe { self.device.GetDeviceRemovedReason() }.is_err()
        }

        pub fn set_sharpness(&mut self, level: u8) {
            if let Some(scaler) = self.scaler.as_mut() {
                scaler.set_sharpness(level);
            }
        }

        /// Draw (or stop drawing) the mouse pointer into captured frames.
        ///
        /// Desktop Duplication never includes the pointer in the desktop image,
        /// so without this the stream shows a cursor-less desktop while every
        /// mouse move still pays for a full encode. Turning it off is only
        /// correct when the client paints its own pointer.
        pub fn set_pointer_overlay(&mut self, enabled: bool) {
            self.pointer_overlay = enabled;
        }

        /// True when the current frame needs re-encoding purely because the
        /// pointer moved: the desktop itself did not change.
        pub fn pointer_moved(&self) -> bool {
            self.pointer_overlay && self.pointer_visible
        }

        /// Refresh the pointer bitmap and position from the acquired frame.
        ///
        /// Must run while the frame is still held: `GetFramePointerShape` is
        /// only valid between `AcquireNextFrame` and `ReleaseFrame`.
        fn update_pointer(
            &mut self,
            dupl: &IDXGIOutputDuplication,
            info: &DXGI_OUTDUPL_FRAME_INFO,
        ) {
            if info.LastMouseUpdateTime == 0 && info.PointerShapeBufferSize == 0 {
                return;
            }
            self.pointer_position = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );
            self.pointer_visible = info.PointerPosition.Visible.as_bool();

            let size = info.PointerShapeBufferSize;
            if size == 0 {
                // Position-only update: the bitmap is unchanged.
                return;
            }
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut pixels = vec![0u8; size as usize];
            let mut required = size;
            let result = unsafe {
                dupl.GetFramePointerShape(
                    size,
                    pixels.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape_info,
                )
            };
            if result.is_err() {
                // Some drivers report a stale size; ask again for the real one.
                required = 0;
                if unsafe {
                    dupl.GetFramePointerShape(
                        0,
                        core::ptr::null_mut(),
                        &mut required,
                        &mut shape_info,
                    )
                }
                .is_err()
                    || required == 0
                {
                    return;
                }
                pixels = vec![0u8; required as usize];
                if unsafe {
                    dupl.GetFramePointerShape(
                        required,
                        pixels.as_mut_ptr().cast(),
                        &mut required,
                        &mut shape_info,
                    )
                }
                .is_err()
                {
                    return;
                }
            }
            pixels.truncate((required as usize).min(pixels.len()));
            let shape = crate::cursor::PointerShape {
                kind: shape_info.Type,
                width: shape_info.Width,
                height: shape_info.Height,
                pitch: shape_info.Pitch,
                hotspot_x: shape_info.HotSpot.x,
                hotspot_y: shape_info.HotSpot.y,
                pixels,
            };
            self.pointer_shape = shape.is_drawable().then_some(shape);
        }

        pub fn waiting_for_desktop(&self) -> bool {
            self.duplication.is_none() && !self.needs_recovery()
        }

        /// Handles static desktop refinement (2 progressive frames after motion stops)
        /// followed by Parsec-style `0.06Mbps` idle heartbeat mode unless `Constant FPS` is enabled.
        fn should_emit_static_frame(&mut self, force_frame: bool) -> bool {
            if !self.has_valid_frame {
                return false;
            }
            if !self.static_screen {
                // Motion just stopped: two progressive refinement frames at a
                // much lower QP, then the idle heartbeat takes over.
                self.static_screen = true;
                self.refinement_frames_left = 2;
                self.refinement_requested = true;
            }
            if self.refinement_frames_left > 0 {
                self.refinement_frames_left -= 1;
                return true;
            }
            if is_constant_fps() || force_frame {
                return true;
            }
            false
        }

        pub fn capture_frame(
            &mut self,
            timeout_ms: u32,
            force_frame: bool,
        ) -> Result<bool, String> {
            if self.duplication.is_none() && !self.reacquire_duplication(Duration::ZERO) {
                return if self.needs_recovery() {
                    Err("Desktop capture device was removed".into())
                } else {
                    Ok(false)
                };
            }
            let Some(dupl) = self.duplication.clone() else {
                return Err("Desktop duplication is unavailable".into());
            };

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut desktop_res: Option<IDXGIResource> = None;
            let res =
                unsafe { dupl.AcquireNextFrame(timeout_ms, &mut frame_info, &mut desktop_res) };

            match res {
                Ok(()) => {
                    let _guard = FrameReleaseGuard(&dupl);
                    self.update_pointer(&dupl, &frame_info);
                    let desktop_changed = frame_info.AccumulatedFrames > 0
                        || frame_info.TotalMetadataBufferSize > 0
                        || frame_info.LastPresentTime != 0
                        || !self.has_valid_frame;
                    let Some(res) = desktop_res else {
                        return Ok(self.should_emit_static_frame(force_frame));
                    };
                    if !desktop_changed {
                        // A pointer move only deserves a frame when the pointer
                        // is part of the picture. Without the overlay the new
                        // frame would be byte-identical to the last one, so the
                        // encode — a full 1080p encode per mouse move — is pure
                        // waste on a host that is already the bottleneck.
                        if frame_info.LastMouseUpdateTime != 0 && self.pointer_moved() {
                            self.static_screen = false;
                            return Ok(self.has_valid_frame);
                        }
                        return Ok(self.should_emit_static_frame(force_frame));
                    }
                    self.static_screen = false;

                    let src_tex: ID3D11Texture2D = res
                        .cast()
                        .map_err(|e| format!("acquired desktop resource is not Texture2D: {e}"))?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { src_tex.GetDesc(&mut desc) };
                    let (width, height) = if matches!(
                        self.rotation,
                        DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
                    ) {
                        (desc.Height, desc.Width)
                    } else {
                        (desc.Width, desc.Height)
                    };
                    if self.has_valid_frame && (desc.Width, desc.Height) != self.surface_size {
                        self.duplication = None;
                        self.has_valid_frame = false;
                        return Err(format!("Display mode changed to {width}x{height}"));
                    }
                    self.surface_size = (desc.Width, desc.Height);

                    if matches!(
                        self.rotation,
                        DXGI_MODE_ROTATION_ROTATE90
                            | DXGI_MODE_ROTATION_ROTATE180
                            | DXGI_MODE_ROTATION_ROTATE270
                    ) {
                        self.scale_cpu_fallback(&src_tex)?;
                    } else if self.encode_width == self.native_width
                        && self.encode_height == self.native_height
                        && desc.Width == self.encode_width
                        && desc.Height == self.encode_height
                    {
                        unsafe {
                            self.context.CopyResource(&self.work_tex, &src_tex);
                        }
                    } else if self
                        .scaler
                        .as_ref()
                        .is_some_and(|scaler| scaler.scale(&src_tex).is_ok())
                    {
                    } else {
                        self.scale_cpu_fallback(&src_tex)?;
                    }
                    self.has_valid_frame = true;
                    // Queue 2 progressive refinement frames so static text snaps to lossless clarity
                    self.refinement_frames_left = 2;
                    Ok(true)
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    Ok(self.should_emit_static_frame(force_frame))
                }
                Err(ref e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.duplication = None;
                    self.has_valid_frame = false;
                    drop(dupl);
                    if self.reacquire_duplication(Duration::from_millis(15)) {
                        return Ok(self.has_valid_frame);
                    }
                    if self.needs_recovery() {
                        Err(format!("Desktop duplication lost access: {e}"))
                    } else {
                        Ok(false)
                    }
                }
                Err(e) => Err(format!("AcquireNextFrame failed: {e}")),
            }
        }

        pub fn reacquire_duplication(&mut self, budget: Duration) -> bool {
            if self.duplication.is_some() {
                return true;
            }
            let Some(output1) = self.output1.clone() else {
                return false;
            };
            let started = Instant::now();
            loop {
                if let Ok(dupl) = unsafe { output1.DuplicateOutput(&self.device) } {
                    self.rotation = unsafe { dupl.GetDesc() }.Rotation;
                    self.duplication = Some(dupl);
                    return true;
                }
                if started.elapsed() >= budget {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn scale_cpu_fallback(&mut self, src_tex: &ID3D11Texture2D) -> Result<(), String> {
            let mut source_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { src_tex.GetDesc(&mut source_desc) };
            let width = source_desc.Width;
            let height = source_desc.Height;
            if width == 0 || height == 0 {
                return Err("Capture source has an empty surface".into());
            }
            if self
                .scale_staging
                .as_ref()
                .is_none_or(|(w, h, _)| (*w, *h) != (width, height))
            {
                self.scale_staging = Some((
                    width,
                    height,
                    create_bgra_texture(&self.device, width, height, true)?,
                ));
            }
            let Some((_, _, staging)) = self.scale_staging.as_ref() else {
                return Err("Capture scale staging texture is unavailable".into());
            };
            let dst_w = self.encode_width as usize;
            let dst_h = self.encode_height as usize;
            self.scale_pixels.resize(dst_w * dst_h * 4, 0);
            unsafe {
                self.context.CopyResource(staging, src_tex);
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }
            .map_err(|e| format!("staging Map failed: {e}"))?;

            let raw_w = source_desc.Width as usize;
            let raw_h = source_desc.Height as usize;
            let (src_w, src_h) = match self.rotation {
                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270 => (raw_h, raw_w),
                _ => (raw_w, raw_h),
            };
            let pitch = mapped.RowPitch as usize;
            let src_ptr = mapped.pData as *const u8;

            for y in 0..dst_h {
                let sy = (y * src_h) / dst_h;
                for x in 0..dst_w {
                    let sx = (x * src_w) / dst_w;
                    let (raw_x, raw_y) = match self.rotation {
                        DXGI_MODE_ROTATION_ROTATE90 => (sy, src_w - 1 - sx),
                        DXGI_MODE_ROTATION_ROTATE180 => (src_w - 1 - sx, src_h - 1 - sy),
                        DXGI_MODE_ROTATION_ROTATE270 => (src_h - 1 - sy, sx),
                        _ => (sx, sy),
                    };
                    let src_off = raw_y.min(raw_h - 1) * pitch + raw_x.min(raw_w - 1) * 4;
                    let dst_off = (y * dst_w + x) * 4;
                    unsafe {
                        let px = core::ptr::read_unaligned(src_ptr.add(src_off) as *const u32);
                        core::ptr::write_unaligned(
                            self.scale_pixels.as_mut_ptr().add(dst_off) as *mut u32,
                            px,
                        );
                    }
                }
            }

            unsafe {
                self.context.Unmap(staging, 0);
                self.context.UpdateSubresource(
                    &self.work_tex,
                    0,
                    None,
                    self.scale_pixels.as_ptr().cast(),
                    (dst_w * 4) as u32,
                    0,
                );
            }
            Ok(())
        }

        pub fn read_bgra_into(&mut self, dst: &mut Vec<u8>) -> Result<(), String> {
            if self.staging_tex.is_none() {
                self.staging_tex = Some(create_bgra_texture(
                    &self.device,
                    self.encode_width,
                    self.encode_height,
                    true,
                )?);
            }
            let Some(ref staging) = self.staging_tex else {
                return Err("missing staging texture".to_string());
            };
            unsafe {
                self.context.CopyResource(staging, &self.work_tex);
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }
            .map_err(|e| format!("Map staging failed: {e}"))?;

            let w = self.encode_width as usize;
            let h = self.encode_height as usize;
            let row_bytes = w * 4;
            let pitch = mapped.RowPitch as usize;
            dst.resize(row_bytes * h, 0);
            let src_ptr = mapped.pData as *const u8;

            for y in 0..h {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src_ptr.add(y * pitch),
                        dst.as_mut_ptr().add(y * row_bytes),
                        row_bytes,
                    );
                }
            }
            unsafe {
                self.context.Unmap(staging, 0);
            }

            // Desktop Duplication leaves the pointer out of the desktop image,
            // so it is composited here — after the frame was scaled to stream
            // size, and with the pointer position scaled by the same factor so
            // it lands where the user's cursor actually is.
            //
            // A rotated output is skipped: the frame has already been rotated
            // in the CPU fallback and the pointer would need the same
            // transform to line up.
            if self.pointer_overlay
                && self.pointer_visible
                && self.rotation == DXGI_MODE_ROTATION_IDENTITY
                && let Some(shape) = self.pointer_shape.as_ref()
            {
                crate::cursor::composite(
                    dst,
                    self.encode_width,
                    self.encode_height,
                    self.pointer_position.0,
                    self.pointer_position.1,
                    self.native_width,
                    self.native_height,
                    shape,
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_resolution_preserves_aspect_without_upscaling() {
        assert_eq!(fit_resolution(1920, 1080, 3840, 2160), (1920, 1080));
        assert_eq!(fit_resolution(2560, 1440, 1920, 1080), (1920, 1080));
        assert_eq!(fit_resolution(1920, 1200, 1920, 1080), (1728, 1080));
        assert_eq!(fit_resolution(3840, 2160, 1280, 720), (1280, 720));
    }
}
