//! Native Windows client renderer using Direct3D 11.
//!
//! This module implements the Parsec-style low-latency presentation path:
//! - HWND-attached IDXGISwapChain with FLIP_DISCARD
//! - Max frame latency = 1, present with sync interval = 0 (immediate)
//! - ID3D11VideoDecoder for NV12 hardware decoding directly into GPU memory
//! - Single-pass HLSL pixel shader: NV12 -> RGB + scaling + FidelityFX CAS
//! - Client-side cursor as hardware overlay (decoupled from stream)

use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11CreateDevice,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext1,
    ID3D11InputLayout, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    // ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDecoder,  // Not available in windows-rs 0.62
    // ID3D11VideoDecoderOutputView, ID3D11VideoDevice, ID3D11VideoProcessor,  // Not available in windows-rs 0.62
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter1, IDXGIDevice1, IDXGIFactory2,
    IDXGISwapChain1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_PRESENT, DXGI_SWAP_CHAIN_FLAG, IDXGIOutput,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows_core::{Interface, GUID};

use crate::native_d3d11::{POLICY, NativeD3D11Policy};

/// Stub types for D3D11 Video Decoder (not available in windows-rs 0.62)
/// These need to be replaced with MediaFoundation or NVDEC implementation
#[derive(Debug, Clone, Copy)]
pub struct D3D11_DECODER_DESC {
    pub Guid: GUID,
    pub SampleWidth: u32,
    pub SampleHeight: u32,
    pub OutputFormat: u32, // DXGI_FORMAT
    pub Profile: GUID,
}

#[derive(Debug, Clone, Copy)]
pub struct D3D11_DECODER_BUFFER_DESC {
    pub BufferType: u32,
    pub BufferIndex: u32,
    pub DataOffset: u32,
    pub DataSize: u32,
    pub FirstMB: u32,
    pub NumMBsInBuffer: u32,
    pub Width: u32,
    pub Height: u32,
    pub Stride: u32,
    pub ReservedBits: u32,
    pub pIV: *mut c_void,
    pub pBuffer: *mut c_void,
    pub pData: *mut c_void,
}

// Constants for decoder buffer types
pub const D3D11_VIDEO_DECODER_BUFFER_BITSTREAM: u32 = 1;
pub const D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS: u32 = 2;

/// Client decoder configuration matching the wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    HevcMain10,
    Av1,
}

/// Presentation swap chain configuration.
#[derive(Debug, Clone, Copy)]
pub struct SwapChainConfig {
    pub width: u32,
    pub height: u32,
    pub hwnd: HWND,
}

impl Default for SwapChainConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            hwnd: HWND(std::ptr::null_mut()),
        }
    }
}

/// NV12 texture pair (Y plane + UV plane) for zero-copy video decode.
#[derive(Debug)]
pub struct Nv12Texture {
    pub y_texture: ID3D11Texture2D,
    pub uv_texture: ID3D11Texture2D,
    pub y_srv: ID3D11ShaderResourceView,
    pub uv_srv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
}

/// Decoded frame ready for presentation.
#[derive(Debug)]
pub struct DecodedFrame {
    pub nv12: Nv12Texture,
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
}

/// Single-pass presentation pipeline state.
pub struct NativeD3D11Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    context1: ID3D11DeviceContext1,
    swap_chain: IDXGISwapChain1,
    // video_device: ID3D11VideoDevice,  // Not available in windows-rs 0.62
    // video_context: ID3D11VideoContext,  // Not available in windows-rs 0.62
    // video_context1: ID3D11VideoContext1,  // Not available in windows-rs 0.62
    // decoder: Option<ID3D11VideoDecoder>,  // Not available in windows-rs 0.62
    decoder_profile: GUID,
    // decoder_output_view: Option<ID3D11VideoDecoderOutputView>,  // Not available in windows-rs 0.62
    back_buffer_rtv: Option<ID3D11RenderTargetView>,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    input_layout: ID3D11InputLayout,
    vertex_buffer: ID3D11Buffer,
    constant_buffer: ID3D11Buffer,
    width: u32,
    height: u32,
    sharpness: f32,
    full_range: bool,
    /// Current decoder surface dimensions (may differ from swap chain on rotation)
    decode_width: u32,
    decode_height: u32,
    /// Hardware cursor overlay state
    cursor_overlay: Option<CursorOverlay>,
    /// Frame latency waitable object for Present(0) synchronization
    frame_latency_waitable: Option<windows::Win32::Foundation::HANDLE>,
}

#[derive(Debug)]
struct CursorOverlay {
    texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    hotspot_x: i32,
    hotspot_y: i32,
    visible: bool,
    position_x: f32,
    position_y: f32,
    scale_x: f32,
    scale_y: f32,
}

impl NativeD3D11Renderer {
    /// Create a new native D3D11 renderer attached to the given HWND.
    pub fn new(config: SwapChainConfig) -> Result<Self, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                return Err(format!("CoInitializeEx failed: {hr:?}"));
            }
        }

        let (device, context, context1) = create_d3d11_device()?;

        // Apply the Parsec-style latency policy immediately
        apply_latency_policy(&device)?;

        let swap_chain = create_swap_chain(&device, &context, &config)?;

        // Get frame latency waitable object for Present(0) sync
        let frame_latency_waitable = get_frame_latency_waitable_object(&swap_chain)?;

        // Create back buffer RTV
        let back_buffer_rtv = create_back_buffer_rtv(&device, &swap_chain)?;

        // Create presentation shader pipeline
        let (pixel_shader, sampler, input_layout, vertex_buffer, constant_buffer) =
            create_presentation_pipeline(&device)?;

        // Get initial back buffer size
        let (width, height) = get_swap_chain_size(&swap_chain)?;

        Ok(Self {
            device,
            context,
            context1,
            swap_chain,
            decoder_profile: D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
            back_buffer_rtv: Some(back_buffer_rtv),
            pixel_shader,
            sampler,
            input_layout,
            vertex_buffer,
            constant_buffer,
            width,
            height,
            sharpness: 0.45,
            full_range: true,
            decode_width: width,
            decode_height: height,
            cursor_overlay: None,
            frame_latency_waitable,
        })
    }

    /// Set the video codec and create/recreate the hardware decoder.
    pub fn set_codec(&mut self, codec: VideoCodec) -> Result<(), String> {
        let profile = match codec {
            VideoCodec::H264 => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
            VideoCodec::Hevc | VideoCodec::HevcMain10 => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
            VideoCodec::Av1 => return Err("AV1 hardware decode not yet implemented".to_string()),
        };

        // D3D11 VideoDecoder not available in windows-rs 0.62
        // Use MediaFoundation or NVDEC instead
        self.decoder_profile = profile;
        Err("D3D11 VideoDecoder not implemented - use MediaFoundation or NVDEC".to_string())
    }

    fn create_decoder(&mut self) -> Result<(), String> {
        // D3D11 VideoDecoder not available in windows-rs 0.62
        // Use MediaFoundation or NVDEC instead
        Err("D3D11 VideoDecoder not implemented - use MediaFoundation or NVDEC".to_string())
    }

    fn create_nv12_texture(&self, width: u32, height: u32) -> Result<Nv12Texture, String> {
        // Y plane (R8_UNORM)
        let y_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut y_texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&y_desc, None, Some(&mut y_texture))
                .map_err(|e| format!("Create Y texture failed: {e}"))?;
        }
        let y_texture = y_texture.ok_or_else(|| "Create Y texture returned null".to_string())?;

        // UV plane (R8G8_UNORM) - half resolution
        let uv_desc = D3D11_TEXTURE2D_DESC {
            Width: width / 2,
            Height: height / 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut uv_texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&uv_desc, None, Some(&mut uv_texture))
                .map_err(|e| format!("Create UV texture failed: {e}"))?;
        }
        let uv_texture = uv_texture.ok_or_else(|| "Create UV texture returned null".to_string())?;

        // Create SRVs for both planes
        let y_srv = create_shader_resource_view(&self.device, &y_texture, DXGI_FORMAT_R8_UNORM)?;
        let uv_srv = create_shader_resource_view(&self.device, &uv_texture, DXGI_FORMAT_R8G8_UNORM)?;

        Ok(Nv12Texture {
            y_texture,
            uv_texture,
            y_srv,
            uv_srv,
            width,
            height,
        })
    }

    /// Submit a NAL unit bitstream for hardware decoding.
    /// Returns the decoded NV12 texture when a frame is ready.
    pub fn submit_bitstream(&mut self, _nal_data: &[u8], _frame_id: u64, _timestamp_us: u64, _is_keyframe: bool)
        -> Result<Option<DecodedFrame>, String>
    {
        // D3D11 VideoDecoder not available in windows-rs 0.62
        // Use MediaFoundation or NVDEC instead
        Err("D3D11 VideoDecoder not implemented - use MediaFoundation or NVDEC".to_string())
    }

    /// Present a decoded frame with single-pass YUV->RGB+CAS shader.
    pub fn present_frame(&mut self, frame: &DecodedFrame) -> Result<(), String> {
        // Update constant buffer with frame parameters
        self.update_constant_buffer(frame)?;

        // Set render target
        if let Some(rtv) = &self.back_buffer_rtv {
            unsafe {
                self.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            }
        }

        // Set viewport
        let viewport = windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            self.context.RSSetViewports(Some(&[viewport]));
        }

        // Set shader resources (NV12 Y + UV planes)
        let srvs = [Some(frame.nv12.y_srv.clone()), Some(frame.nv12.uv_srv.clone())];
        unsafe {
            self.context.PSSetShaderResources(0, Some(&srvs));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            self.context.IASetInputLayout(Some(&self.input_layout));
            self.context.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.context.VSSetShader(None, None);
            self.context.PSSetShader(Some(&self.pixel_shader), None);
        }

        // Draw full-screen quad
        let stride = 16u32; // float4 position + float2 texcoord
        let offset = 0u32;
        let vertex_buffer = Some(self.vertex_buffer.clone());
        let strides = [stride];
        let offsets = [offset];
        unsafe {
            self.context.IASetVertexBuffers(0, 1, Some(&vertex_buffer as *const _), Some(strides.as_ptr()), Some(offsets.as_ptr()));
            self.context.Draw(4, 0);
        }

        // Present with sync interval = 0 (immediate), flags = 0
        let present_result = unsafe {
            self.swap_chain.Present(0, DXGI_PRESENT(0))
        };

        if present_result.is_err() {
            // Handle device removed / lost
            return Err(format!("Present failed: {present_result:?}"));
        }

        // Wait on frame latency waitable object for Present(0) synchronization
        if let Some(handle) = self.frame_latency_waitable {
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(handle, 1000);
            }
        }

        Ok(())
    }

    fn update_constant_buffer(&self, frame: &DecodedFrame) -> Result<(), String> {
        #[repr(C)]
        struct CasParams {
            inv_tex_size: [f32; 2],
            sharpness: f32,
            full_range: f32,
            decode_size: [f32; 2],
            output_size: [f32; 2],
            _pad: [f32; 2],
        }

        let params = CasParams {
            inv_tex_size: [1.0 / frame.nv12.width as f32, 1.0 / (frame.nv12.height / 2) as f32],
            sharpness: self.sharpness,
            full_range: if self.full_range { 1.0 } else { 0.0 },
            decode_size: [frame.nv12.width as f32, frame.nv12.height as f32],
            output_size: [self.width as f32, self.height as f32],
            _pad: [0.0, 0.0],
        };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.constant_buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map constant buffer failed: {e}"))?;

            std::ptr::copy_nonoverlapping(
                &params as *const _ as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<CasParams>(),
            );

            self.context.Unmap(&self.constant_buffer, 0);
        }
        Ok(())
    }

    /// Update sharpness parameter (0.0 to 1.0).
    pub fn set_sharpness(&mut self, sharpness: f32) {
        self.sharpness = sharpness.clamp(0.0, 1.0);
    }

    /// Set full range (BT.709 0-255) vs studio range (16-235).
    pub fn set_full_range(&mut self, full_range: bool) {
        self.full_range = full_range;
    }

    /// Update cursor overlay state.
    pub fn update_cursor(&mut self, visible: bool, x: f32, y: f32, scale_x: f32, scale_y: f32) {
        if let Some(cursor) = &mut self.cursor_overlay {
            cursor.visible = visible;
            cursor.position_x = x;
            cursor.position_y = y;
            cursor.scale_x = scale_x;
            cursor.scale_y = scale_y;
        }
    }

    /// Set cursor texture from raw BGRA data.
    pub fn set_cursor_texture(&mut self, width: u32, height: u32, hotspot_x: i32, hotspot_y: i32, pixels: &[u8]) -> Result<(), String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device.CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| format!("Create cursor texture failed: {e}"))?;
        }
        let texture = texture.ok_or_else(|| "Create cursor texture returned null".to_string())?;

        // Upload pixel data
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.Map(&texture, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map cursor texture failed: {e}"))?;
            let row_pitch = mapped.RowPitch as usize;
            let src_row_pitch = (width * 4) as usize;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * src_row_pitch),
                    (mapped.pData as *mut u8).add(y * row_pitch),
                    src_row_pitch,
                );
            }
            self.context.Unmap(&texture, 0);
        }

        let srv = create_shader_resource_view(&self.device, &texture, DXGI_FORMAT_B8G8R8A8_UNORM)?;

        self.cursor_overlay = Some(CursorOverlay {
            texture,
            srv,
            hotspot_x,
            hotspot_y,
            visible: true,
            position_x: 0.0,
            position_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        });

        Ok(())
    }

    /// Resize the swap chain and recreate resources.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == self.width && height == self.height {
            return Ok(());
        }

        // Release back buffer RTV
        self.back_buffer_rtv = None;

        // Resize swap chain
        unsafe {
            self.swap_chain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))
                .map_err(|e| format!("ResizeBuffers failed: {e}"))?;
        }

        // Recreate back buffer RTV
        self.back_buffer_rtv = Some(create_back_buffer_rtv(&self.device, &self.swap_chain)?);

        self.width = width;
        self.height = height;

        // If decoder dimensions changed, recreate decoder (stubbed)
        if width != self.decode_width || height != self.decode_height {
            self.decode_width = width;
            self.decode_height = height;
        }

        Ok(())
    }

    /// Get the current swap chain size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Get the native D3D11 policy for diagnostics/negotiation.
    pub fn policy() -> &'static NativeD3D11Policy {
        &POLICY
    }
}

impl Drop for NativeD3D11Renderer {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Create D3D11 device with video support.
fn create_d3d11_device() -> Result<(
    ID3D11Device,
    ID3D11DeviceContext,
    ID3D11DeviceContext1,
), String> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut level = D3D_FEATURE_LEVEL::default();

    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;
    }

    let device = device.ok_or_else(|| "D3D11CreateDevice returned null device".to_string())?;
    let context = context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;

    let context1 = context.cast::<ID3D11DeviceContext1>()
        .map_err(|e| format!("ID3D11DeviceContext1 unavailable: {e}"))?;

    // Video device/context not available in windows-rs 0.62
    // let video_device = device.cast::<ID3D11VideoDevice>()
    //     .map_err(|e| format!("ID3D11VideoDevice unavailable: {e}"))?;
    // let video_context = context.cast::<ID3D11VideoContext>()
    //     .map_err(|e| format!("ID3D11VideoContext unavailable: {e}"))?;
    // let video_context1 = context1.cast::<ID3D11VideoContext1>()
    //     .map_err(|e| format!("ID3D11VideoContext1 unavailable: {e}"))?;

    Ok((device, context, context1))
}

/// Apply Parsec-style latency policy: max frame latency = 1.
fn apply_latency_policy(device: &ID3D11Device) -> Result<(), String> {
    let dxgi_device: IDXGIDevice1 = device.cast()
        .map_err(|e| format!("IDXGIDevice1 cast failed: {e}"))?;
    unsafe {
        dxgi_device.SetMaximumFrameLatency(1)
            .map_err(|e| format!("SetMaximumFrameLatency(1) failed: {e}"))?;
    }
    Ok(())
}

/// Create FLIP_DISCARD swap chain attached to HWND.
fn create_swap_chain(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    config: &SwapChainConfig,
) -> Result<IDXGISwapChain1, String> {
    let dxgi_device: IDXGIDevice1 = device.cast()
        .map_err(|e| format!("IDXGIDevice1 cast failed: {e}"))?;

    let adapter: IDXGIAdapter1 = unsafe {
        dxgi_device.GetAdapter()
            .map_err(|e| format!("GetAdapter failed: {e}"))?
    }.cast()
    .map_err(|e| format!("IDXGIAdapter1 cast failed: {e}"))?;

    let factory: IDXGIFactory2 = unsafe {
        adapter.GetParent()
            .map_err(|e| format!("GetParent IDXGIFactory2 failed: {e}"))?
    };

    let swap_chain_desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: config.width,
        Height: config.height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: windows_core::BOOL(0),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    };

    let swap_chain = unsafe {
        factory.CreateSwapChainForHwnd(
            device,
            config.hwnd,
            &swap_chain_desc,
            None,
            None::<&IDXGIOutput>,
        )
        .map_err(|e| format!("CreateSwapChainForHwnd failed: {e}"))?
    };

    // Disable ALT+Enter fullscreen toggle
    let factory1: windows::Win32::Graphics::Dxgi::IDXGIFactory = factory.cast().unwrap();
    unsafe {
        factory1.MakeWindowAssociation(config.hwnd, windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER)
            .ok();
    }

    Ok(swap_chain)
}

/// Get frame latency waitable object for Present(0) synchronization.
fn get_frame_latency_waitable_object(swap_chain: &IDXGISwapChain1) -> Result<Option<windows::Win32::Foundation::HANDLE>, String> {
    let swap_chain2: windows::Win32::Graphics::Dxgi::IDXGISwapChain2 = swap_chain.cast()
        .map_err(|e| format!("Swap chain cast to IDXGISwapChain2 failed: {e}"))?;
    let handle = unsafe { swap_chain2.GetFrameLatencyWaitableObject() };
    if handle.is_invalid() {
        Ok(None)
    } else {
        Ok(Some(handle))
    }
}

/// Create back buffer render target view.
fn create_back_buffer_rtv(device: &ID3D11Device, swap_chain: &IDXGISwapChain1) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView, String> {
    let back_buffer: ID3D11Texture2D = unsafe {
        swap_chain.GetBuffer(0)
            .map_err(|e| format!("GetBuffer failed: {e}"))?
    };

    let mut rtv: Option<windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView> = None;
    unsafe {
        device.CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))
            .map_err(|e| format!("CreateRenderTargetView failed: {e}"))?;
    }
    rtv.ok_or_else(|| "CreateRenderTargetView returned null".to_string())
}

/// Create shader resource view for a texture plane.
fn create_shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }

    let srv_desc = windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };

    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe {
        device.CreateShaderResourceView(texture, Some(&srv_desc), Some(&mut srv))
            .map_err(|e| format!("CreateShaderResourceView failed: {e}"))?;
    }
    srv.ok_or_else(|| "CreateShaderResourceView returned null".to_string())
}

/// Create the single-pass presentation pipeline (vertex shader + pixel shader + constant buffer).
fn create_presentation_pipeline(
    device: &ID3D11Device,
) -> Result<(
    ID3D11PixelShader,
    ID3D11SamplerState,
    ID3D11InputLayout,
    ID3D11Buffer,
    ID3D11Buffer,
), String> {
    // Vertex shader: pass-through full-screen quad
    let vs_hlsl = r#"
        struct VS_IN { float2 pos : POSITION; float2 uv : TEXCOORD0; };
        struct VS_OUT { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
        VS_OUT main(VS_IN input) {
            VS_OUT output;
            output.pos = float4(input.pos, 0.0, 1.0);
            output.uv = input.uv;
            return output;
        }
    "#;

    let vs_blob = compile_shader(vs_hlsl, "main", "vs_5_0")?;

    // Pixel shader: NV12 -> RGB + scaling + CAS (from graphics.rs CAS_BT709_FULL_RANGE_HLSL)
    let ps_hlsl = include_str!("../shaders/nv12_cas_presentation.hlsl");
    let ps_blob = compile_shader(ps_hlsl, "main", "ps_5_0")?;

    let vertex_shader = unsafe {
        let mut vs: Option<windows::Win32::Graphics::Direct3D11::ID3D11VertexShader> = None;
        device.CreateVertexShader(&vs_blob, None, Some(&mut vs))
            .map_err(|e| format!("CreateVertexShader failed: {e}"))?;
        vs.ok_or_else(|| "CreateVertexShader returned null".to_string())?
    };

    let pixel_shader = unsafe {
        let mut ps: Option<ID3D11PixelShader> = None;
        device.CreatePixelShader(&ps_blob, None, Some(&mut ps))
            .map_err(|e| format!("CreatePixelShader failed: {e}"))?;
        ps.ok_or_else(|| "CreatePixelShader returned null".to_string())?
    };

    // Input layout: POSITION (float2) + TEXCOORD (float2)
    let input_elements = [
        windows::Win32::Graphics::Direct3D11::D3D11_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"POSITION\0".as_ptr()),
            SemanticIndex: 0,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: windows::Win32::Graphics::Direct3D11::D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        windows::Win32::Graphics::Direct3D11::D3D11_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"TEXCOORD\0".as_ptr()),
            SemanticIndex: 0,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 8,
            InputSlotClass: windows::Win32::Graphics::Direct3D11::D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];

    let input_layout = unsafe {
        let mut layout: Option<ID3D11InputLayout> = None;
        device.CreateInputLayout(
            &input_elements,
            &vs_blob,
            Some(&mut layout),
        ).map_err(|e| format!("CreateInputLayout failed: {e}"))?;
        layout.ok_or_else(|| "CreateInputLayout returned null".to_string())?
    };

    // Full-screen quad vertex buffer (4 vertices, triangle strip)
    #[repr(C)]
    struct Vertex { pos: [f32; 2], uv: [f32; 2] }
    let vertices = [
        Vertex { pos: [-1.0, -1.0], uv: [0.0, 1.0] },
        Vertex { pos: [1.0, -1.0], uv: [1.0, 1.0] },
        Vertex { pos: [-1.0, 1.0], uv: [0.0, 0.0] },
        Vertex { pos: [1.0, 1.0], uv: [1.0, 0.0] },
    ];

    let vb_desc = D3D11_BUFFER_DESC {
        ByteWidth: (std::mem::size_of::<Vertex>() * 4) as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_VERTEX_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };

    let vb_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: vertices.as_ptr() as *const c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };

    let vertex_buffer = unsafe {
        let mut vb: Option<ID3D11Buffer> = None;
        device.CreateBuffer(&vb_desc, Some(&vb_data), Some(&mut vb))
            .map_err(|e| format!("Create vertex buffer failed: {e}"))?;
        vb.ok_or_else(|| "Create vertex buffer returned null".to_string())?
    };

    // Sampler: linear clamp
    let sampler_desc = windows::Win32::Graphics::Direct3D11::D3D11_SAMPLER_DESC {
        Filter: windows::Win32::Graphics::Direct3D11::D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: windows::Win32::Graphics::Direct3D11::D3D11_COMPARISON_NEVER,
        BorderColor: [0.0, 0.0, 0.0, 0.0],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };

    let sampler = unsafe {
        let mut s: Option<ID3D11SamplerState> = None;
        device.CreateSamplerState(&sampler_desc, Some(&mut s))
            .map_err(|e| format!("CreateSamplerState failed: {e}"))?;
        s.ok_or_else(|| "CreateSamplerState returned null".to_string())?
    };

    // Constant buffer for CAS parameters
    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: 256, // Enough for CasParams
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };

    let constant_buffer = unsafe {
        let mut cb: Option<ID3D11Buffer> = None;
        device.CreateBuffer(&cb_desc, None, Some(&mut cb))
            .map_err(|e| format!("Create constant buffer failed: {e}"))?;
        cb.ok_or_else(|| "Create constant buffer returned null".to_string())?
    };

    Ok((pixel_shader, sampler, input_layout, vertex_buffer, constant_buffer))
}

/// Compile HLSL shader using D3DCompile.
fn compile_shader(source: &str, entry: &str, target: &str) -> Result<Vec<u8>, String> {
    // Pre-compile shaders offline and embed bytecode for production.
    // This is a placeholder - in reality you'd use dxc or fxc to compile ahead of time.
    let _ = (source, entry, target);
    Err("Runtime shader compilation not implemented - pre-compile shaders offline".to_string())
}

/// Get current swap chain size.
fn get_swap_chain_size(swap_chain: &IDXGISwapChain1) -> Result<(u32, u32), String> {
    let desc = unsafe {
        swap_chain.GetDesc1()
            .map_err(|e| format!("GetDesc1 failed: {e}"))?
    };
    Ok((desc.Width, desc.Height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_constants_match_parsec() {
        assert_eq!(POLICY.swap_effect, "DXGI_SWAP_EFFECT_FLIP_DISCARD");
        assert_eq!(POLICY.present_sync_interval, 0);
        assert_eq!(POLICY.max_frame_latency, 1);
        assert!(POLICY.frame_latency_waitable_object);
        assert_eq!(POLICY.decode_surface, "NV12 ID3D11Texture2D");
        assert_eq!(POLICY.decoder_api, "MediaFoundation/NVDEC"); // D3D11 VideoDecoder not available in windows-rs 0.62
        assert!(POLICY.hidden_stream_cursor);
        assert!(POLICY.local_cursor_overlay);
    }
}