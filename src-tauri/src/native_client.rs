//! Windows viewer: Media Foundation decodes onto D3D11 surfaces, which the
//! video processor renders into an HWND swap chain on the same device.

use crate::{
    capture::win::VideoScaler,
    protocol::{ReassembledFrame, VideoCodec},
};
use std::{
    mem::ManuallyDrop,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{
        CloseHandle, DXGI_STATUS_OCCLUDED, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0,
        WPARAM,
    },
    Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::*,
        Dxgi::{Common::*, *},
        Gdi::{CombineRgn, CreateRectRgn, DeleteObject, RGN_DIFF, SetWindowRgn},
    },
    Media::MediaFoundation::*,
    System::{
        Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
        Threading::WaitForSingleObject,
    },
    UI::WindowsAndMessaging::*,
};
use windows_core::{Interface, w};

#[path = "native_client/cas.rs"]
mod cas;

use crate::native_d3d11::SurfaceLayout;

#[derive(Clone, Copy, Debug, Default)]
pub struct Playback {
    pub size: Option<(u32, u32)>,
    pub decoded: u64,
    pub presented: u64,
    pub dropped: u64,
    pub decode_us: u64,
    pub render_submit_us: u64,
    pub render_attempts: u64,
    pub failed: bool,
    pub needs_keyframe: bool,
}

#[derive(Default)]
struct Pending {
    frame: Option<ReassembledFrame>,
    stopped: bool,
    visible: bool,
    sharpness: u8,
    playback: Playback,
}

pub struct Viewer {
    hwnd: usize,
    pending: Arc<(Mutex<Pending>, Condvar)>,
}

impl Viewer {
    // Must be called on the window's UI thread.
    pub fn new(parent: HWND) -> Result<Self, String> {
        let hwnd = create_child(parent).map_err(|e| e.to_string())?;
        let raw = hwnd.0 as usize;
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let input = pending.clone();
        if let Err(error) = thread::Builder::new()
            .name("vmax-d3d11-viewer".into())
            .spawn(move || {
                let hwnd = HWND(raw as *mut _);
                if let Err(error) = play(hwnd, &input) {
                    tracing::warn!(%error, "Windows native playback switched to WebCodecs");
                    input.0.lock().unwrap().playback.failed = true;
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_APP + 1, WPARAM(0), LPARAM(0)) };
                }
            })
        {
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(error.to_string());
        }
        Ok(Self { hwnd: raw, pending })
    }

    pub fn layout(&self, layout: SurfaceLayout) -> Result<(), String> {
        let hwnd = HWND(self.hwnd as *mut _);
        let picture = layout.picture;
        unsafe {
            let visible = layout.visible && picture.width > 0 && picture.height > 0;
            {
                let mut pending = self.pending.0.lock().unwrap();
                pending.visible = visible;
                pending.sharpness = layout.sharpness.min(100);
            }
            if !visible || self.playback().failed {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return Ok(());
            }
            // Clear rectangles occupied by interactive WebView controls from the HWND.
            let region = CreateRectRgn(0, 0, picture.width, picture.height);
            if region.is_invalid() {
                return Err("Could not clip the native video window".into());
            }
            for rect in layout.controls.into_iter().take(32) {
                let left = rect.x.saturating_sub(picture.x);
                let top = rect.y.saturating_sub(picture.y);
                let exclusion = CreateRectRgn(
                    left,
                    top,
                    left.saturating_add(rect.width),
                    top.saturating_add(rect.height),
                );
                if !exclusion.is_invalid() {
                    CombineRgn(Some(region), Some(region), Some(exclusion), RGN_DIFF);
                    let _ = DeleteObject(exclusion.into());
                }
            }
            if SetWindowRgn(hwnd, Some(region), true) == 0 {
                let _ = DeleteObject(region.into());
                return Err("Could not apply the native video clip".into());
            }
            let flags = SWP_NOACTIVATE
                | if self.playback().size.is_some() {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                };
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                picture.x,
                picture.y,
                picture.width,
                picture.height,
                flags,
            )
            .map_err(|e| e.to_string())
        }
    }

    pub fn offer(&self, frame: &ReassembledFrame) -> bool {
        let mut input = self.pending.0.lock().unwrap();
        if input.playback.failed || input.stopped {
            return false;
        }
        if input.frame.replace(frame.clone()).is_some() {
            input.playback.dropped += 1;
        }
        self.pending.1.notify_one();
        true
    }

    pub fn playback(&self) -> Playback {
        self.pending.0.lock().unwrap().playback
    }

    pub fn take_keyframe_request(&self) -> bool {
        std::mem::take(&mut self.pending.0.lock().unwrap().playback.needs_keyframe)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let mut pending = self.pending.0.lock().unwrap();
        pending.stopped = true;
        pending.frame = None;
        self.pending.1.notify_one();
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

unsafe extern "system" fn child_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        msg if msg == WM_APP + 1 => {
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn create_child(parent: HWND) -> windows_core::Result<HWND> {
    const CLASS: windows_core::PCWSTR = w!("VmaXNativeVideo");
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    unsafe {
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        if !REGISTERED.get_or_init(|| {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(child_proc),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            }) != 0
        }) {
            return Err(windows_core::Error::from_thread());
        }
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            CLASS,
            w!(""),
            WS_CHILD | WS_CLIPSIBLINGS | WS_DISABLED,
            0,
            0,
            1,
            1,
            Some(parent),
            None,
            Some(instance),
            None,
        )
    }
}

struct MediaFoundation;
impl MediaFoundation {
    fn start() -> windows_core::Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
                CoUninitialize();
                return Err(error);
            }
        }
        Ok(Self)
    }
}
impl Drop for MediaFoundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Format {
    codec: VideoCodec,
    width: u32,
    height: u32,
    full_range: bool,
}

fn play(hwnd: HWND, input: &(Mutex<Pending>, Condvar)) -> Result<(), String> {
    let _mf = MediaFoundation::start().map_err(|e| e.to_string())?;
    let mut output = Renderer::new(hwnd).map_err(|e| e.to_string())?;
    let mut decoder: Option<(Format, Decoder)> = None;
    let mut previous = None;
    let mut decode_stalled = None;
    let mut presentation_stalled = None;
    loop {
        let (frame, sharpness) = {
            let mut pending = input
                .1
                .wait_while(input.0.lock().unwrap(), |pending| {
                    !pending.stopped && pending.frame.is_none()
                })
                .unwrap();
            if pending.stopped {
                return Ok(());
            }
            (pending.frame.take().unwrap(), pending.sharpness)
        };
        let format = Format {
            codec: VideoCodec::from_u8(frame.codec).ok_or("Unknown video codec")?,
            width: u32::from(frame.meta.width),
            height: u32::from(frame.meta.height),
            full_range: frame.meta.is_full_range(),
        };
        let stalled_since = decode_stalled.get_or_insert_with(Instant::now);
        if !(16..=8192).contains(&format.width) || !(16..=8192).contains(&format.height) {
            return Err("Unsupported video dimensions".into());
        }
        let gap = previous.is_some_and(|id: u32| id.wrapping_add(1) != frame.frame_id);
        previous = Some(frame.frame_id);
        if gap
            || decoder
                .as_ref()
                .is_some_and(|(active, _)| *active != format)
        {
            decoder = None;
        }
        if decoder.is_none() {
            if !frame.is_keyframe {
                input.0.lock().unwrap().playback.needs_keyframe = true;
                if stalled_since.elapsed() >= Duration::from_secs(2) {
                    return Err("Decoder timed out waiting for a keyframe".into());
                }
                continue;
            }
            decoder = Some((
                format,
                Decoder::new(&output.device, format).map_err(|e| e.to_string())?,
            ));
        }
        let started = Instant::now();
        let Some(sample) = decoder
            .as_mut()
            .unwrap()
            .1
            .decode(&frame)
            .map_err(|e| e.to_string())?
        else {
            if stalled_since.elapsed() >= Duration::from_secs(2) {
                return Err("Hardware decoder produced no output".into());
            }
            continue;
        };
        decode_stalled = None;
        let decode_us = started.elapsed().as_micros() as u64;
        crate::capture::record_decode_latency_us(decode_us.min(u64::from(u32::MAX)) as u32);
        let render_started = Instant::now();
        let displayed = output.present(&sample, format, sharpness)?;
        let render_submit_us = render_started.elapsed().as_micros() as u64;
        let mut state = input.0.lock().unwrap();
        state.playback.decoded += 1;
        state.playback.decode_us += decode_us;
        state.playback.render_submit_us += render_submit_us;
        state.playback.render_attempts += 1;
        state.playback.size = Some((format.width, format.height));
        if displayed {
            state.playback.presented += 1;
            presentation_stalled = None;
        } else {
            state.playback.dropped += 1;
            if !state.visible {
                presentation_stalled = None;
            } else if presentation_stalled
                .get_or_insert_with(Instant::now)
                .elapsed()
                >= Duration::from_secs(2)
            {
                return Err("D3D11 swap chain produced no output".into());
            }
        }
    }
}

struct Decoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    output_type: windows_core::GUID,
}

impl Decoder {
    fn new(device: &ID3D11Device, format: Format) -> windows_core::Result<Self> {
        let (subtype, output_type) = match format.codec {
            VideoCodec::H264 => (MFVideoFormat_H264, MFVideoFormat_NV12),
            VideoCodec::Hevc => (MFVideoFormat_HEVC, MFVideoFormat_NV12),
            VideoCodec::HevcMain10 => (MFVideoFormat_HEVC, MFVideoFormat_P010),
            _ => {
                return Err(windows_core::Error::new(
                    MF_E_INVALIDMEDIATYPE,
                    "No native decoder for this codec",
                ));
            }
        };
        unsafe {
            let mut manager = None;
            let mut token = 0;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: subtype,
            };
            let mut entries = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut entries,
                &mut count,
            )?;
            let mut selected = None;
            if count > 0 && !entries.is_null() {
                for entry in std::slice::from_raw_parts_mut(entries, count as usize) {
                    let Some(entry) = entry.take() else { continue };
                    if selected.is_none() {
                        selected =
                            Self::configure(&entry, &manager, subtype, output_type, format).ok();
                    }
                }
            }
            CoTaskMemFree(Some(entries.cast()));
            let transform = selected.ok_or_else(|| {
                windows_core::Error::new(
                    MF_E_INVALIDMEDIATYPE,
                    "No DXVA-capable decoder accepted the stream",
                )
            })?;
            Ok(Self {
                transform,
                _manager: manager,
                output_type,
            })
        }
    }

    fn configure(
        entry: &IMFActivate,
        manager: &IMFDXGIDeviceManager,
        subtype: windows_core::GUID,
        output_type: windows_core::GUID,
        format: Format,
    ) -> windows_core::Result<IMFTransform> {
        unsafe {
            let transform: IMFTransform = entry.ActivateObject()?;
            let attrs = transform.GetAttributes()?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or_default() == 0 {
                return Err(windows_core::Error::from_hresult(MF_E_INVALIDMEDIATYPE));
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            input.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(format.width) << 32) | u64::from(format.height),
            )?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let nominal_range = if format.full_range {
                MFNominalRange_0_255
            } else {
                MFNominalRange_16_235
            };
            for (attribute, value) in [
                (MF_MT_VIDEO_NOMINAL_RANGE, nominal_range.0),
                (MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0),
                (MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0),
                (MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0),
            ] {
                input.SetUINT32(&attribute, value as u32)?;
            }
            transform.SetInputType(0, &input, 0)?;
            Self::select_output(&transform, output_type)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(transform)
        }
    }

    fn select_output(
        transform: &IMFTransform,
        subtype: windows_core::GUID,
    ) -> windows_core::Result<()> {
        unsafe {
            for index in 0.. {
                let candidate = transform.GetOutputAvailableType(0, index)?;
                if candidate.GetGUID(&MF_MT_SUBTYPE)? == subtype {
                    return transform.SetOutputType(0, &candidate, 0);
                }
            }
        }
        unreachable!()
    }

    fn decode(&self, frame: &ReassembledFrame) -> windows_core::Result<Option<IMFSample>> {
        unsafe {
            let sample = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(frame.payload.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(frame.payload.as_ptr(), ptr, frame.payload.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(frame.payload.len() as u32)?;
            sample.AddBuffer(&buffer)?;
            sample
                .SetSampleTime(frame.timestamp_us.saturating_mul(10).min(i64::MAX as u64) as i64)?;
            if frame.is_keyframe {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            let mut latest = None;
            if let Err(error) = self.transform.ProcessInput(0, &sample, 0) {
                if error.code() != MF_E_NOTACCEPTING {
                    return Err(error);
                }
                latest = self.drain()?;
                self.transform.ProcessInput(0, &sample, 0)?;
            }
            Ok(self.drain()?.or(latest))
        }
    }

    fn drain(&self) -> windows_core::Result<Option<IMFSample>> {
        let mut latest = None;
        for _ in 0..16 {
            let mut output = MFT_OUTPUT_DATA_BUFFER::default();
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut 0)
            };
            let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
            drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
            match result {
                Ok(()) => {
                    if sample.is_some() {
                        latest = sample;
                    }
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    Self::select_output(&self.transform, self.output_type)?
                }
                Err(error) => return Err(error),
            }
        }
        Ok(latest)
    }
}

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain2,
    ready: HANDLE,
    flags: DXGI_SWAP_CHAIN_FLAG,
    present_flags: DXGI_PRESENT,
    converter: Option<(Format, VideoScaler, Option<u8>)>,
    cas_shaders: Option<cas::Shaders>,
    cas_pass: Option<cas::Pass>,
}

impl Renderer {
    fn new(hwnd: HWND) -> windows_core::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            let _ = context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let mut allow_tearing = windows_core::BOOL(0);
            if let Ok(factory5) = factory.cast::<IDXGIFactory5>() {
                let _ = factory5.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    (&mut allow_tearing as *mut windows_core::BOOL).cast(),
                    std::mem::size_of_val(&allow_tearing) as u32,
                );
            }
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
            let mut present_flags = DXGI_PRESENT_DO_NOT_WAIT;
            if allow_tearing.as_bool() {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
                present_flags |= DXGI_PRESENT_ALLOW_TEARING;
            }
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 1,
                Height: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags.0 as u32,
                ..Default::default()
            };
            let swap_chain: IDXGISwapChain2 = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?
                .cast()?;
            swap_chain.SetMaximumFrameLatency(crate::native_d3d11::MAX_FRAME_LATENCY)?;
            let ready = swap_chain.GetFrameLatencyWaitableObject();
            if ready.is_invalid() {
                return Err(windows_core::Error::from_thread());
            }
            let cas_shaders = cas::Shaders::new(&device)
                .map_err(|error| tracing::warn!(%error, "Native CAS shader unavailable"))
                .ok();
            Ok(Self {
                device,
                context,
                swap_chain,
                ready,
                flags,
                present_flags,
                converter: None,
                cas_shaders,
                cas_pass: None,
            })
        }
    }

    fn present(
        &mut self,
        sample: &IMFSample,
        format: Format,
        sharpness: u8,
    ) -> Result<bool, String> {
        unsafe {
            if WaitForSingleObject(self.ready, 0) != WAIT_OBJECT_0 {
                return Ok(false);
            }
            let buffer = sample
                .GetBufferByIndex(0)
                .and_then(|b| b.cast::<IMFDXGIBuffer>())
                .map_err(|_| "Decoder output is not a DXGI surface".to_string())?;
            let mut raw = std::ptr::null_mut();
            buffer
                .GetResource(&ID3D11Texture2D::IID, &mut raw)
                .map_err(|e| e.to_string())?;
            if raw.is_null() {
                return Err("Decoder returned an empty DXGI surface".into());
            }
            let texture = ID3D11Texture2D::from_raw(raw);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let slice =
                buffer.GetSubresourceIndex().map_err(|e| e.to_string())? / desc.MipLevels.max(1);
            if self
                .converter
                .as_ref()
                .is_none_or(|(current, _, _)| *current != format)
            {
                self.converter = None;
                self.cas_pass = None;
                self.context.ClearState();
                self.swap_chain
                    .ResizeBuffers(
                        2,
                        format.width,
                        format.height,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        self.flags,
                    )
                    .map_err(|e| e.to_string())?;
                let target = self.swap_chain.GetBuffer(0).map_err(|e| e.to_string())?;
                self.cas_pass = self.cas_shaders.as_ref().and_then(|_| {
                    cas::Pass::new(&self.device, &target, format.width, format.height)
                        .map_err(|error| tracing::warn!(%error, "Native CAS surface unavailable"))
                        .ok()
                });
                let video_target = self.cas_pass.as_ref().map_or(&target, |pass| &pass.input);
                let converter = VideoScaler::new(
                    &self.device,
                    &self.context,
                    video_target,
                    format.width,
                    format.height,
                    format.width,
                    format.height,
                )?;
                converter.set_yuv_input(format.full_range)?;
                self.converter = Some((format, converter, None));
            }
            let (_, converter, applied_sharpness) = self.converter.as_mut().unwrap();
            if self.cas_pass.is_none() && *applied_sharpness != Some(sharpness) {
                converter.set_sharpness(sharpness);
                *applied_sharpness = Some(sharpness);
            }
            converter.scale_slice(&texture, slice)?;
            if let (Some(pass), Some(shaders)) = (&mut self.cas_pass, &self.cas_shaders) {
                pass.draw(&self.context, shaders, sharpness);
            }
            let status = self.swap_chain.Present(
                crate::native_d3d11::PRESENT_SYNC_INTERVAL,
                self.present_flags,
            );
            if status == DXGI_ERROR_WAS_STILL_DRAWING || status == DXGI_STATUS_OCCLUDED {
                return Ok(false);
            }
            status.ok().map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.ready) };
    }
}
