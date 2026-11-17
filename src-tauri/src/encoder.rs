use crate::protocol::VideoCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderModePreference {
    Hardware,
    Software,
}

pub struct EncodedAccessUnit {
    pub codec: VideoCodec,
    pub is_keyframe: bool,
    pub payload: Vec<u8>,
    /// Wall-clock host encode time in microseconds: colour conversion + NVENC
    /// submit through `NvEncLockBitstream` returning. Travels to the client in
    /// the frame telemetry header.
    pub host_encode_us: u32,
}

/// Checks if a given Annex-B byte stream contains a keyframe (IDR + sequence headers).
pub fn is_annex_b_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 < data.len() {
        let start_len = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            3
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            4
        } else {
            i += 1;
            continue;
        };
        let nalu_idx = i + start_len;
        if nalu_idx < data.len() {
            let hdr = data[nalu_idx];
            match codec {
                VideoCodec::H264 => {
                    let nalu_type = hdr & 0x1F;
                    if nalu_type == 5 || nalu_type == 7 {
                        return true;
                    }
                }
                VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                    let nalu_type = (hdr >> 1) & 0x3F;
                    if (16..=21).contains(&nalu_type) || nalu_type == 32 || nalu_type == 33 {
                        return true;
                    }
                }
                // AV1 has no start codes: see `is_av1_keyframe`.
                VideoCodec::Av1 => return false,
            }
        }
        i = nalu_idx + 1;
    }
    false
}

/// Reads an AV1 `leb128` length, returning the value and the byte after it.
fn read_leb128(data: &[u8], mut index: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(index)?;
        index += 1;
        value |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// True when an AV1 temporal unit starts with a shown key frame.
///
/// AV1 is a length-delimited OBU stream, not Annex-B, so the H.26x NAL scan
/// cannot see it. Only the first frame-header OBU matters: `show_existing_frame`
/// reuses an earlier picture, otherwise the two bits that follow are
/// `frame_type`, and `KEY_FRAME` is zero.
pub fn is_av1_keyframe(data: &[u8]) -> bool {
    let mut index = 0usize;
    while index < data.len() {
        let header = data[index];
        if header & 0x80 != 0 {
            // Forbidden bit set: not an OBU stream this parser understands.
            return false;
        }
        let obu_type = (header >> 3) & 0x0F;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut cursor = index + 1;
        if has_extension {
            cursor += 1;
        }
        let size = if has_size {
            match read_leb128(data, cursor) {
                Some((value, next)) => {
                    cursor = next;
                    value
                }
                None => return false,
            }
        } else {
            0
        };
        match obu_type {
            // OBU_FRAME_HEADER / OBU_FRAME: the picture type lives here.
            3 | 6 => {
                let Some(first) = data.get(cursor) else {
                    return false;
                };
                if first & 0x80 != 0 {
                    return false;
                }
                return (first >> 5) & 0x03 == 0;
            }
            // Temporal delimiter, sequence header, metadata, padding: skip.
            _ => {}
        }
        if !has_size {
            return false;
        }
        index = cursor.saturating_add(size);
    }
    false
}

/// True when an access unit carries a key frame, for any codec the host emits.
pub fn is_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::Av1 => is_av1_keyframe(data),
        _ => is_annex_b_keyframe(codec, data),
    }
}

/// True BT.709 Full-Range (0–255) BGRA8 -> NV12 conversion with 2x2 box-filtered chroma.
/// Eliminates the washed-out grey levels and colored text fringing of BT.601 studio-range math.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, nv12_out: &mut Vec<u8>) {
    // Single implementation, shared with the GPU shader's math and pinned by
    // `color::tests`. Keeping a second copy of the coefficients here is how
    // BT.601 studio-range math crept in before.
    crate::color::bgra_to_nv12_bt709(bgra, width, height, nv12_out);
}

#[cfg(target_os = "windows")]
pub mod win {
    use super::{EncodedAccessUnit, EncoderModePreference, bgra_to_nv12, is_keyframe};
    use crate::bitrate::FramePacing;
    use crate::capture::win::{
        DxgiCapturer, NVIDIA_VENDOR_ID, VideoScaler, create_bgra_texture, create_capture_texture,
        create_texture, open_nvidia_d3d11_device,
    };
    use crate::protocol::VideoCodec;
    use core::ffi::c_void;
    use mini_nvcodec::guids::{
        NV_ENC_AV1_PROFILE_MAIN_GUID, NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID,
        NV_ENC_CODEC_HEVC_GUID, NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_HEVC_PROFILE_FREXT_GUID,
        NV_ENC_HEVC_PROFILE_MAIN_GUID, NV_ENC_HEVC_PROFILE_MAIN10_GUID, NV_ENC_PRESET_P1_GUID,
        NV_ENC_PRESET_P3_GUID,
    };
    use mini_nvcodec::nvenc_sys::*;
    use mini_nvcodec::nvenc_ver::{API_VERSION_LADDER, NvencApiVersion};
    use mini_nvcodec::{NvencApi, try_init_nvenc};
    use std::sync::Once;
    use std::time::Instant;
    use tracing::warn;
    use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DInclude};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_UNORDERED_ACCESS, D3D11_TEX2D_UAV1, D3D11_UAV_DIMENSION_TEXTURE2D,
        D3D11_UNORDERED_ACCESS_VIEW_DESC1, D3D11_UNORDERED_ACCESS_VIEW_DESC1_0,
        ID3D11ComputeShader, ID3D11Device, ID3D11Device3, ID3D11DeviceContext,
        ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM,
        DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R16_UNORM,
        DXGI_FORMAT_R16G16_UNORM,
    };
    use windows::Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
        CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
        CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncH264CABACEnable,
        CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
        CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
        IMFMediaBuffer, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_LOW_LATENCY,
        MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
        MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
        MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_VERSION,
        MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
        MFNominalRange_0_255, MFSTARTUP_NOSOCKET, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
        MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO,
        MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
        MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
        MFVideoTransferMatrix_BT709,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::Variant::{
        VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
    };
    use windows_core::{GUID, Interface};

    /// Official NVIDIA NV_ENC_PRESET_P4_GUID: {90A7B826-DF06-4862-B9D2-CD6D73A08681}
    /// Gives Parsec-equivalent visual quality at ~2.45ms encode latency on RTX 3050.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawGuid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    fn preset_p4_guid() -> mini_nvcodec::nvenc_sys::GUID {
        let raw = RawGuid {
            data1: 0x90a7_b826,
            data2: 0xdf06,
            data3: 0x4862,
            data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
        };
        unsafe { core::mem::transmute_copy(&raw) }
    }

    /// Applies Parsec-grade NVENC Rate Control & VUI Bitstream Restrictions directly to the
    /// NVIDIA C-ABI structs:
    /// - `enableMinQP = 1`, `minQP = {min_qp, min_qp, min_qp}` (prevents bitrate waste on flat regions)
    /// - `zeroReorderDelay = 1`, `enableAQ = 1`, `aqStrength = 8`
    /// - `multiPass = NV_ENC_TWO_PASS_QUARTER_RESOLUTION` (1)
    /// - VUI: `videoFullRangeFlag = 1` (0–255), BT.709 color matrix (1), `numReorderFrames = 0`,
    ///   `maxDecFrameBuffering = 1` (forces client hardware decoders to output with zero DPB queue delay).
    /// Write the [`crate::nvenc::NvencPlan`] into the real `NV_ENC_*` structs.
    ///
    /// Every field below is a named struct member from the NVENC SDK header, so
    /// the values cannot land on the wrong offset as the struct evolves. The
    /// previous version poked at raw pointer offsets, which silently did nothing
    /// on any SDK layout other than the one it was written against.
    unsafe fn apply_nvenc_plan(
        encode_config: &mut NV_ENC_CONFIG,
        codec: VideoCodec,
        plan: &crate::nvenc::NvencPlan,
    ) {
        let rc = &mut encode_config.rcParams;
        rc.multiPass = if plan.rate_control.multi_pass_quarter_res {
            NV_ENC_MULTI_PASS::TWO_PASS_QUARTER_RESOLUTION
        } else {
            NV_ENC_MULTI_PASS::DISABLED
        };
        rc.set_enableLookahead(0);
        rc.set_zeroReorderDelay(1);
        rc.lookaheadDepth = plan.rate_control.lookahead_depth;
        rc.set_enableAQ(u32::from(plan.rate_control.aq_strength != 0));
        rc.set_aqStrength(u32::from(plan.rate_control.aq_strength.clamp(1, 15)));
        rc.set_enableMinQP(u32::from(plan.rate_control.min_qp_inter_p < 51));
        rc.minQP = NV_ENC_QP {
            qpInterP: u32::from(plan.rate_control.min_qp_inter_p),
            qpInterB: u32::from(plan.rate_control.min_qp_inter_p),
            qpIntra: u32::from(plan.rate_control.min_qp_intra),
        };
        // No reordering anywhere in the pipeline: one reference, zero B
        // frames, one decoded-picture slot on the client.
        encode_config.frameIntervalP = plan.frame_interval_p as i32;
        encode_config.gopLength = plan.gop_length;

        let vui = plan.vui;
        match codec {
            VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                let hevc = unsafe { &mut encode_config.encodeCodecConfig.hevcConfig };
                hevc.maxNumRefFramesInDPB = 1;
                hevc.numRefL0 = NV_ENC_NUM_REF_FRAMES::REF_1;
                hevc.numRefL1 = NV_ENC_NUM_REF_FRAMES::REF_1;
                hevc.set_repeatSPSPPS(u32::from(plan.repeat_sps_pps));
                hevc.hevcVUIParameters = vui_params(vui);
                if codec == VideoCodec::Hevc444 {
                    hevc.hevcVUIParameters.chromaSampleLocationFlag = 0;
                }
            }
            VideoCodec::H264 => {
                let h264 = unsafe { &mut encode_config.encodeCodecConfig.h264Config };
                h264.maxNumRefFrames = 1;
                h264.numRefL0 = NV_ENC_NUM_REF_FRAMES::REF_1;
                h264.numRefL1 = NV_ENC_NUM_REF_FRAMES::REF_1;
                h264.set_repeatSPSPPS(u32::from(plan.repeat_sps_pps));
                h264.h264VUIParameters = vui_params(vui);
            }
            VideoCodec::Av1 => {
                let av1 = unsafe { &mut encode_config.encodeCodecConfig.av1Config };
                av1.maxNumRefFramesInDPB = 1;
                av1.numFwdRefs = NV_ENC_NUM_REF_FRAMES::REF_1;
                av1.numBwdRefs = NV_ENC_NUM_REF_FRAMES::REF_1;
                av1.set_repeatSeqHdr(u32::from(plan.repeat_sps_pps));
                // AV1 signals colour on the config, not through a VUI struct.
                av1.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES(i32::from(vui.colour_primaries));
                av1.transferCharacteristics =
                    NV_ENC_VUI_TRANSFER_CHARACTERISTIC(i32::from(vui.transfer_characteristics));
                av1.matrixCoefficients =
                    NV_ENC_VUI_MATRIX_COEFFS(i32::from(vui.matrix_coefficients));
                av1.colorRange = u32::from(vui.video_full_range);
            }
        }
    }

    fn vui_params(vui: crate::nvenc::VuiBt709FullRange) -> NV_ENC_CONFIG_H264_VUI_PARAMETERS {
        let mut params: NV_ENC_CONFIG_H264_VUI_PARAMETERS = unsafe { core::mem::zeroed() };
        params.videoSignalTypePresentFlag = u32::from(vui.video_signal_type_present);
        // 5 = unspecified/component: the colour description below is the
        // authoritative signalling.
        params.videoFormat = NV_ENC_VUI_VIDEO_FORMAT(5);
        params.videoFullRangeFlag = u32::from(vui.video_full_range);
        params.colourDescriptionPresentFlag = u32::from(vui.colour_description_present);
        params.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES(i32::from(vui.colour_primaries));
        params.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC(i32::from(vui.transfer_characteristics));
        params.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS(i32::from(vui.matrix_coefficients));
        params.chromaSampleLocationFlag = u32::from(vui.chroma_sample_location_flag);
        params.chromaSampleLocationTop = u32::from(vui.chroma_sample_location_top);
        params.chromaSampleLocationBot = u32::from(vui.chroma_sample_location_bot);
        params.bitstreamRestrictionFlag = u32::from(vui.bitstream_restriction);
        params
    }

    pub trait WinVideoEncoder: Send {
        fn codec(&self) -> VideoCodec;
        fn pipeline_label(&self) -> &str;
        fn hardware_load_hint(&self) -> bool;
        fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), String>;
        fn force_keyframe(&mut self);
        /// Congestion-driven bitrate change: applies on the next P-frame with
        /// no encoder reset and no forced IDR.
        fn reconfigure_bitrate(&mut self, bitrate_bps: u32, _fps: u32) -> Result<(), String> {
            self.set_bitrate_bps(bitrate_bps)
        }
        /// Ask for N near-lossless refinement frames. Used when the desktop
        /// goes static so text snaps back to full clarity.
        fn request_refinement(&mut self, _frames: u32) {}
        fn last_encode_latency_ms(&self) -> f32 {
            crate::capture::latest_encode_latency_ms()
        }
        /// Start an intra-refresh wave. Encoders without a rolling refresh
        /// implementation fall back to a keyframe, which is still better than
        /// leaving the decoder on a broken reference chain.
        fn request_intra_refresh(&mut self, _frames: u32) {
            self.force_keyframe();
        }
        fn encode(
            &mut self,
            capturer: &mut DxgiCapturer,
            timestamp_us: u64,
        ) -> Result<Option<EncodedAccessUnit>, String>;
    }

    pub fn create_encoder(
        capturer: &DxgiCapturer,
        mode: EncoderModePreference,
        preferred_codecs: &[VideoCodec],
        fps: u16,
        bitrate_bps: u32,
        frame_pacing: FramePacing,
        nvenc: crate::nvenc::NvencSettings,
    ) -> Result<Box<dyn WinVideoEncoder>, String> {
        let codecs = if preferred_codecs.is_empty() {
            vec![VideoCodec::Hevc, VideoCodec::H264]
        } else {
            preferred_codecs.to_vec()
        };
        // Detail pacing (or a persisted P3 preference) is the one signal that
        // says "spend more encoder time on this frame". Both the NVENC plan and
        // the Media Foundation rate control read it, so a host without an
        // NVIDIA GPU gets the same intent instead of a vendor default.
        let quality_first = frame_pacing == FramePacing::Detail || nvenc.preset == 3;

        if mode == EncoderModePreference::Hardware {
            let p4 = preset_p4_guid();
            let presets = if nvenc.preset == 3 {
                [NV_ENC_PRESET_P3_GUID, p4, NV_ENC_PRESET_P1_GUID]
            } else {
                [p4, NV_ENC_PRESET_P3_GUID, NV_ENC_PRESET_P1_GUID]
            };
            for &codec in &codecs {
                for preset in presets {
                    match NvencD3d11Encoder::new(capturer, codec, fps, bitrate_bps, preset, nvenc) {
                        Ok(enc) => return Ok(Box::new(enc)),
                        Err(e) => {
                            warn!(?codec, error = %e, "NVENC init failed, trying next option")
                        }
                    }
                }
            }
            for &codec in &codecs {
                match MftVideoEncoder::new(
                    capturer.encode_width,
                    capturer.encode_height,
                    fps,
                    bitrate_bps,
                    codec,
                    false,
                    quality_first,
                ) {
                    Ok(enc) => return Ok(Box::new(enc)),
                    Err(e) => warn!(?codec, error = %e, "HW MFT init failed"),
                }
            }
        }

        let mut ordered = codecs;
        ordered.sort_by_key(|codec| *codec != VideoCodec::H264);
        for codec in ordered {
            match MftVideoEncoder::new(
                capturer.encode_width,
                capturer.encode_height,
                fps,
                bitrate_bps,
                codec,
                true,
                quality_first,
            ) {
                Ok(enc) => return Ok(Box::new(enc)),
                Err(e) => warn!(?codec, error = %e, "SW MFT init failed"),
            }
        }

        Err("no hardware or software video encoder could initialize".to_string())
    }

    pub struct NvencD3d11Encoder {
        api: &'static NvencApi,
        encoder: *mut c_void,
        rung: NvencApiVersion,
        codec: VideoCodec,
        pipeline_label: String,
        is_zero_copy: bool,
        _nv_device: ID3D11Device,
        nv_context: ID3D11DeviceContext,
        input_texture: ID3D11Texture2D,
        _yuv_texture: ID3D11Texture2D,
        yuv_converter: YuvConverter,
        registered_res: NV_ENC_REGISTERED_PTR,
        bitstream_buf: NV_ENC_OUTPUT_PTR,
        init_params: NV_ENC_INITIALIZE_PARAMS,
        encode_config: NV_ENC_CONFIG,
        width: u32,
        height: u32,
        frame_idx: u32,
        force_idr_next: bool,
        force_ir_frames: u32,
        intra_refresh: crate::encoder_recovery::IntraRefreshConfig,
        cpu_staging: Vec<u8>,
        last_encode_ms: f32,
        plan: crate::nvenc::NvencPlan,
        settings: crate::nvenc::NvencSettings,
        refine_frames: u32,
    }

    enum YuvConverter {
        Compute {
            shader: ID3D11ComputeShader,
            src: [Option<ID3D11ShaderResourceView>; 1],
            planes: [Option<ID3D11UnorderedAccessView>; 3],
        },
        VideoProcessor(VideoScaler),
    }

    impl YuvConverter {
        fn compute(
            device: &ID3D11Device,
            source: &ID3D11Texture2D,
            width: u32,
            height: u32,
            codec: VideoCodec,
        ) -> Result<(ID3D11Texture2D, Self), String> {
            let device3 = device.cast::<ID3D11Device3>().map_err(|e| e.to_string())?;
            let (format, view_formats, entry) = match codec {
                VideoCodec::Hevc444 => (
                    DXGI_FORMAT_AYUV,
                    [None, None, Some(DXGI_FORMAT_R8G8B8A8_UNORM)],
                    windows_core::s!("ayuv"),
                ),
                VideoCodec::HevcMain10 => (
                    DXGI_FORMAT_P010,
                    [
                        Some(DXGI_FORMAT_R16_UNORM),
                        Some(DXGI_FORMAT_R16G16_UNORM),
                        None,
                    ],
                    windows_core::s!("p010"),
                ),
                _ => (
                    DXGI_FORMAT_NV12,
                    [
                        Some(DXGI_FORMAT_R8_UNORM),
                        Some(DXGI_FORMAT_R8G8_UNORM),
                        None,
                    ],
                    windows_core::s!("nv12"),
                ),
            };
            let output = create_texture(
                device,
                width,
                height,
                format,
                false,
                D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            )?;
            let mut planes = [None, None, None];
            for (index, format) in view_formats.into_iter().enumerate() {
                let Some(format) = format else { continue };
                let plane = if codec == VideoCodec::Hevc444 {
                    0
                } else {
                    index as u32
                };
                let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC1 {
                    Format: format,
                    ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC1_0 {
                        Texture2D: D3D11_TEX2D_UAV1 {
                            MipSlice: 0,
                            PlaneSlice: plane,
                        },
                    },
                };
                let mut view = None;
                unsafe {
                    device3.CreateUnorderedAccessView1(&output, Some(&desc), Some(&mut view))
                }
                .map_err(|e| format!("YUV plane {index} UAV: {e}"))?;
                planes[index] = Some(
                    view.ok_or("No YUV UAV")?
                        .cast()
                        .map_err(|e| e.to_string())?,
                );
            }
            let mut src = None;
            unsafe { device.CreateShaderResourceView(source, None, Some(&mut src)) }
                .map_err(|e| e.to_string())?;
            let source_code = crate::color::NV12_CSC_HLSL.as_bytes();
            let mut code = None;
            unsafe {
                D3DCompile(
                    source_code.as_ptr().cast(),
                    source_code.len(),
                    windows_core::s!("nv12_csc.hlsl"),
                    None,
                    None::<&ID3DInclude>,
                    entry,
                    windows_core::s!("cs_5_0"),
                    0,
                    0,
                    &mut code,
                    None,
                )
            }
            .map_err(|e| format!("YUV compute shader compilation: {e}"))?;
            let code = code.ok_or("No YUV compute bytecode")?;
            let bytecode = unsafe {
                core::slice::from_raw_parts(
                    code.GetBufferPointer().cast::<u8>(),
                    code.GetBufferSize(),
                )
            };
            let mut shader = None;
            unsafe { device.CreateComputeShader(bytecode, None, Some(&mut shader)) }
                .map_err(|e| e.to_string())?;
            Ok((
                output,
                Self::Compute {
                    shader: shader.ok_or("No YUV compute shader")?,
                    src: [src],
                    planes,
                },
            ))
        }

        fn convert(
            &self,
            context: &ID3D11DeviceContext,
            source: &ID3D11Texture2D,
            width: u32,
            height: u32,
        ) -> Result<(), String> {
            match self {
                Self::Compute {
                    shader,
                    src,
                    planes,
                } => unsafe {
                    context.CSSetShader(shader, None);
                    context.CSSetShaderResources(0, Some(src));
                    context.CSSetUnorderedAccessViews(0, 3, Some(planes.as_ptr()), None);
                    context.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
                    context.CSSetUnorderedAccessViews(
                        0,
                        3,
                        Some([None, None, None].as_ptr()),
                        None,
                    );
                    context.CSSetShaderResources(0, Some(&[None]));
                    context.CSSetShader(None, None);
                    Ok(())
                },
                Self::VideoProcessor(processor) => processor.scale(source),
            }
        }
    }

    unsafe impl Send for NvencD3d11Encoder {}

    impl NvencD3d11Encoder {
        pub fn new(
            capturer: &DxgiCapturer,
            codec: VideoCodec,
            fps: u16,
            bitrate_bps: u32,
            preset_guid: mini_nvcodec::nvenc_sys::GUID,
            settings: crate::nvenc::NvencSettings,
        ) -> Result<Self, String> {
            let api =
                try_init_nvenc().ok_or_else(|| "nvEncodeAPI64.dll not available".to_string())?;

            let (is_zero_copy, nv_device, nv_context, input_texture) =
                if capturer.adapter_vendor_id == NVIDIA_VENDOR_ID {
                    (
                        true,
                        capturer.device.clone(),
                        capturer.context.clone(),
                        capturer.texture().clone(),
                    )
                } else {
                    let (dev, ctx, _) = open_nvidia_d3d11_device()?;
                    let tex = create_bgra_texture(
                        &dev,
                        capturer.encode_width,
                        capturer.encode_height,
                        false,
                    )?;
                    (false, dev, ctx, tex)
                };

            let main10 = codec == VideoCodec::HevcMain10;
            let full_chroma = codec == VideoCodec::Hevc444;
            let width = capturer.encode_width;
            let height = capturer.encode_height;
            let (yuv_texture, yuv_converter) =
                match YuvConverter::compute(&nv_device, &input_texture, width, height, codec) {
                    Ok(conversion) => conversion,
                    Err(err) if !full_chroma => {
                        tracing::debug!(%err, "Using D3D11 video processor for full-range BT.709");
                        let format = if main10 {
                            DXGI_FORMAT_P010
                        } else {
                            DXGI_FORMAT_NV12
                        };
                        let texture =
                            create_capture_texture(&nv_device, width, height, false, format)?;
                        let processor = VideoScaler::new(
                            &nv_device,
                            &nv_context,
                            &texture,
                            width,
                            height,
                            width,
                            height,
                        )?;
                        (texture, YuvConverter::VideoProcessor(processor))
                    }
                    Err(err) => return Err(err),
                };
            let mut encoder: *mut c_void = core::ptr::null_mut();
            let mut negotiated: Option<NvencApiVersion> = None;

            for rung in API_VERSION_LADDER {
                if rung > api.created_with {
                    continue;
                }
                let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { core::mem::zeroed() };
                open.version = rung.open_encode_session_ex_params_ver();
                open.deviceType = NV_ENC_DEVICE_TYPE::DIRECTX;
                open.device = nv_device.as_raw();
                open.apiVersion = rung.nvencapi_version();

                let status = unsafe { (api.nvEncOpenEncodeSessionEx)(&mut open, &mut encoder) };
                if status == NVENCSTATUS::SUCCESS {
                    negotiated = Some(rung);
                    break;
                }
                if !encoder.is_null() {
                    unsafe { (api.nvEncDestroyEncoder)(encoder) };
                    encoder = core::ptr::null_mut();
                }
                if status != NVENCSTATUS::ERR_INVALID_VERSION {
                    return Err(format!("nvEncOpenEncodeSessionEx failed: {status:?}"));
                }
            }

            let Some(rung) = negotiated else {
                return Err("NVENC driver rejected all supported API versions".to_string());
            };

            let codec_guid = match codec {
                VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                    NV_ENC_CODEC_HEVC_GUID
                }
                VideoCodec::H264 => NV_ENC_CODEC_H264_GUID,
                VideoCodec::Av1 => NV_ENC_CODEC_AV1_GUID,
            };
            if main10 {
                let mut caps: NV_ENC_CAPS_PARAM = unsafe { core::mem::zeroed() };
                caps.version = rung.caps_param_ver();
                caps.capsToQuery = NV_ENC_CAPS::SUPPORT_10BIT_ENCODE;
                let mut supported = 0;
                if rung.major_minor() < (12, 2)
                    || unsafe {
                        (api.nvEncGetEncodeCaps)(encoder, codec_guid, &mut caps, &mut supported)
                    } != NVENCSTATUS::SUCCESS
                    || supported == 0
                {
                    unsafe { (api.nvEncDestroyEncoder)(encoder) };
                    return Err("NVENC Main10 encoding is unavailable".to_string());
                }
            }
            if full_chroma {
                let mut caps: NV_ENC_CAPS_PARAM = unsafe { core::mem::zeroed() };
                caps.version = rung.caps_param_ver();
                caps.capsToQuery = NV_ENC_CAPS::SUPPORT_YUV444_ENCODE;
                let mut supported = 0;
                let status = unsafe {
                    (api.nvEncGetEncodeCaps)(encoder, codec_guid, &mut caps, &mut supported)
                };
                if status != NVENCSTATUS::SUCCESS || supported == 0 {
                    unsafe { (api.nvEncDestroyEncoder)(encoder) };
                    return Err("NVENC HEVC 4:4:4 encoding is unavailable".into());
                }
            }
            let tuning = NV_ENC_TUNING_INFO::ULTRA_LOW_LATENCY;

            let mut preset_cfg: NV_ENC_PRESET_CONFIG = unsafe { core::mem::zeroed() };
            preset_cfg.version = rung.preset_config_ver();
            preset_cfg.presetCfg.version = rung.config_ver();

            let s = unsafe {
                (api.nvEncGetEncodePresetConfigEx)(
                    encoder,
                    codec_guid,
                    preset_guid,
                    tuning,
                    &mut preset_cfg,
                )
            };
            if s != NVENCSTATUS::SUCCESS {
                unsafe { (api.nvEncDestroyEncoder)(encoder) };
                return Err(format!(
                    "nvEncGetEncodePresetConfigEx failed for {codec:?}: {s:?}"
                ));
            }

            let mut encode_config = preset_cfg.presetCfg;
            encode_config.version = rung.config_ver();
            let mut intra_refresh = crate::encoder_recovery::IntraRefreshConfig {
                enabled: settings.intra_refresh,
                period_frames: settings.refresh_period.clamp(2, 600),
                refresh_frames: settings.refresh_frames.clamp(1, 60),
            }
            .sanitized();
            let mut caps: NV_ENC_CAPS_PARAM = unsafe { core::mem::zeroed() };
            caps.version = rung.caps_param_ver();
            caps.capsToQuery = NV_ENC_CAPS::SUPPORT_INTRA_REFRESH;
            let mut supported = 0;
            intra_refresh.enabled = intra_refresh.enabled
                && unsafe {
                    (api.nvEncGetEncodeCaps)(encoder, codec_guid, &mut caps, &mut supported)
                } == NVENCSTATUS::SUCCESS
                && supported != 0;

            encode_config.gopLength = u32::MAX;
            encode_config.frameIntervalP = 1; // Strict IPP, zero B-frames
            encode_config.rcParams.version = rung.rc_params_ver();
            encode_config.rcParams.rateControlMode = if settings.vbr {
                NV_ENC_PARAMS_RC_MODE::VBR
            } else {
                NV_ENC_PARAMS_RC_MODE::CBR
            };
            encode_config.rcParams.averageBitRate = bitrate_bps;
            encode_config.rcParams.maxBitRate = settings.max_bitrate(bitrate_bps);

            let frame_bits = settings.vbv_bits(bitrate_bps, u32::from(fps));
            encode_config.rcParams.vbvBufferSize = frame_bits;
            encode_config.rcParams.vbvInitialDelay = frame_bits;

            match codec {
                VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                    encode_config.profileGUID = if full_chroma {
                        NV_ENC_HEVC_PROFILE_FREXT_GUID
                    } else if main10 {
                        NV_ENC_HEVC_PROFILE_MAIN10_GUID
                    } else {
                        NV_ENC_HEVC_PROFILE_MAIN_GUID
                    };
                    unsafe {
                        if full_chroma {
                            encode_config
                                .encodeCodecConfig
                                .hevcConfig
                                .set_chromaFormatIDC(3);
                        }
                        if main10 {
                            encode_config.encodeCodecConfig.hevcConfig.inputBitDepth =
                                NV_ENC_BIT_DEPTH::DEPTH_10;
                            encode_config.encodeCodecConfig.hevcConfig.outputBitDepth =
                                NV_ENC_BIT_DEPTH::DEPTH_10;
                        }
                        encode_config.encodeCodecConfig.hevcConfig.idrPeriod = u32::MAX;
                        encode_config
                            .encodeCodecConfig
                            .hevcConfig
                            .set_enableIntraRefresh(u32::from(intra_refresh.enabled));
                        encode_config
                            .encodeCodecConfig
                            .hevcConfig
                            .intraRefreshPeriod = intra_refresh.period_frames;
                        encode_config.encodeCodecConfig.hevcConfig.intraRefreshCnt =
                            intra_refresh.refresh_frames;
                        encode_config
                            .encodeCodecConfig
                            .hevcConfig
                            .set_repeatSPSPPS(1);
                    }
                }
                VideoCodec::H264 => {
                    encode_config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
                    unsafe {
                        encode_config.encodeCodecConfig.h264Config.idrPeriod = u32::MAX;
                        encode_config
                            .encodeCodecConfig
                            .h264Config
                            .set_enableIntraRefresh(u32::from(intra_refresh.enabled));
                        encode_config
                            .encodeCodecConfig
                            .h264Config
                            .intraRefreshPeriod = intra_refresh.period_frames;
                        encode_config.encodeCodecConfig.h264Config.intraRefreshCnt =
                            intra_refresh.refresh_frames;
                        encode_config
                            .encodeCodecConfig
                            .h264Config
                            .set_repeatSPSPPS(1);
                    }
                }
                VideoCodec::Av1 => {
                    encode_config.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
                    unsafe {
                        let av1 = &mut encode_config.encodeCodecConfig.av1Config;
                        // Main profile is 8-bit 4:2:0; NVENC has no 10-bit or
                        // 4:4:4 AV1 encoder, so this path is exact, not lossy.
                        av1.set_chromaFormatIDC(1);
                        av1.idrPeriod = u32::MAX;
                        av1.set_enableIntraRefresh(u32::from(intra_refresh.enabled));
                        av1.intraRefreshPeriod = intra_refresh.period_frames;
                        av1.intraRefreshCnt = intra_refresh.refresh_frames;
                        // A client that joins or recovers mid-stream needs the
                        // sequence header in band, exactly like the repeat
                        // VPS/SPS path on the HEVC and H.264 branches.
                        av1.set_repeatSeqHdr(1);
                    }
                }
            }

            let mut init_params: NV_ENC_INITIALIZE_PARAMS = unsafe { core::mem::zeroed() };
            init_params.version = rung.initialize_params_ver();
            init_params.encodeGUID = codec_guid;
            init_params.presetGUID = preset_guid;
            init_params.encodeWidth = capturer.encode_width;
            init_params.encodeHeight = capturer.encode_height;
            init_params.darWidth = capturer.encode_width;
            init_params.darHeight = capturer.encode_height;
            init_params.frameRateNum = fps.max(1) as u32;
            init_params.frameRateDen = 1;
            init_params.enableEncodeAsync = 0;
            init_params.enablePTD = 1;
            init_params.maxEncodeWidth = capturer.encode_width;
            init_params.maxEncodeHeight = capturer.encode_height;
            init_params.tuningInfo = tuning;

            let mut plan = crate::nvenc::NvencPlan::streaming(
                u32::from(fps),
                bitrate_bps,
                settings.preset == 3,
            );
            plan.rate_control.vbr = settings.vbr;
            plan.rate_control.vbv_bits = frame_bits;
            plan.rate_control.aq_strength = settings.aq_strength.min(15);
            plan.rate_control.min_qp_intra = settings.min_qp.min(51);
            plan.rate_control.min_qp_inter_p = settings.min_qp.min(51);
            plan.rate_control.multi_pass_quarter_res = settings.quarter_pass;
            plan.intra_refresh.enabled = intra_refresh.enabled;
            plan.intra_refresh.period = intra_refresh.period_frames;
            plan.intra_refresh.count = intra_refresh.refresh_frames;
            let backup_config = encode_config;
            unsafe {
                apply_nvenc_plan(&mut encode_config, codec, &plan);
            }

            init_params.encodeConfig = &mut encode_config;
            let mut s = unsafe { (api.nvEncInitializeEncoder)(encoder, &mut init_params) };
            if s != NVENCSTATUS::SUCCESS {
                // Drivers that reject quarter-resolution multipass can still
                // retain the same color space, rate control and IP-only GOP.
                encode_config = backup_config;
                plan.rate_control.multi_pass_quarter_res = false;
                unsafe { apply_nvenc_plan(&mut encode_config, codec, &plan) };
                init_params.encodeConfig = &mut encode_config;
                s = unsafe { (api.nvEncInitializeEncoder)(encoder, &mut init_params) };
            }
            if s != NVENCSTATUS::SUCCESS {
                plan.rate_control.aq_strength = 0;
                unsafe { apply_nvenc_plan(&mut encode_config, codec, &plan) };
                init_params.encodeConfig = &mut encode_config;
                s = unsafe { (api.nvEncInitializeEncoder)(encoder, &mut init_params) };
            }
            if s != NVENCSTATUS::SUCCESS && settings.vbr {
                encode_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::CBR;
                plan.rate_control.vbr = false;
                init_params.encodeConfig = &mut encode_config;
                s = unsafe { (api.nvEncInitializeEncoder)(encoder, &mut init_params) };
            }
            if s != NVENCSTATUS::SUCCESS {
                unsafe { (api.nvEncDestroyEncoder)(encoder) };
                return Err(format!("nvEncInitializeEncoder failed: {s:?}"));
            }

            init_params.encodeConfig = core::ptr::null_mut();

            let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { core::mem::zeroed() };
            reg.version = rung.register_resource_ver();
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::DIRECTX;
            reg.width = capturer.encode_width;
            reg.height = capturer.encode_height;
            reg.pitch = 0;
            reg.resourceToRegister = yuv_texture.as_raw();
            reg.bufferFormat = if full_chroma {
                NV_ENC_BUFFER_FORMAT::AYUV
            } else if main10 {
                NV_ENC_BUFFER_FORMAT::YUV420_10BIT
            } else {
                NV_ENC_BUFFER_FORMAT::NV12
            };
            reg.bufferUsage = NV_ENC_BUFFER_USAGE::INPUT_IMAGE;

            let s = unsafe { (api.nvEncRegisterResource)(encoder, &mut reg) };
            if s != NVENCSTATUS::SUCCESS {
                unsafe { (api.nvEncDestroyEncoder)(encoder) };
                return Err(format!("nvEncRegisterResource failed: {s:?}"));
            }

            let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { core::mem::zeroed() };
            bs.version = rung.create_bitstream_buffer_ver();
            let s = unsafe { (api.nvEncCreateBitstreamBuffer)(encoder, &mut bs) };
            if s != NVENCSTATUS::SUCCESS {
                unsafe {
                    (api.nvEncUnregisterResource)(encoder, reg.registeredResource);
                    (api.nvEncDestroyEncoder)(encoder);
                }
                return Err(format!("nvEncCreateBitstreamBuffer failed: {s:?}"));
            }

            let codec_label = match codec {
                VideoCodec::Hevc => "H.265",
                VideoCodec::HevcMain10 => "H.265 10-bit",
                VideoCodec::Hevc444 => "H.265",
                VideoCodec::H264 => "H.264",
                VideoCodec::Av1 => "AV1",
            };
            let chroma = if full_chroma { "4:4:4" } else { "4:2:0" };
            let pipeline_label = format!(
                "Hardware {codec_label} {}x{} {chroma} Full Range (Opus Audio)",
                capturer.encode_width, capturer.encode_height
            );

            Ok(Self {
                api,
                encoder,
                rung,
                codec,
                pipeline_label,
                is_zero_copy,
                _nv_device: nv_device,
                nv_context,
                input_texture,
                _yuv_texture: yuv_texture,
                yuv_converter,
                registered_res: reg.registeredResource,
                bitstream_buf: bs.bitstreamBuffer,
                init_params,
                encode_config,
                width: capturer.encode_width,
                height: capturer.encode_height,
                frame_idx: 0,
                force_idr_next: true,
                force_ir_frames: 0,
                intra_refresh,
                cpu_staging: Vec::new(),
                last_encode_ms: 0.0,
                settings: crate::nvenc::NvencSettings {
                    aq_strength: plan.rate_control.aq_strength,
                    ..settings
                },
                plan,
                refine_frames: 0,
            })
        }
    }

    impl Drop for NvencD3d11Encoder {
        fn drop(&mut self) {
            unsafe {
                if !self.bitstream_buf.is_null() {
                    let _ =
                        (self.api.nvEncDestroyBitstreamBuffer)(self.encoder, self.bitstream_buf);
                }
                if !self.registered_res.is_null() {
                    let _ = (self.api.nvEncUnregisterResource)(self.encoder, self.registered_res);
                }
                if !self.encoder.is_null() {
                    let _ = (self.api.nvEncDestroyEncoder)(self.encoder);
                }
            }
        }
    }

    impl WinVideoEncoder for NvencD3d11Encoder {
        fn codec(&self) -> VideoCodec {
            self.codec
        }

        fn pipeline_label(&self) -> &str {
            &self.pipeline_label
        }

        fn hardware_load_hint(&self) -> bool {
            true
        }

        fn last_encode_latency_ms(&self) -> f32 {
            self.last_encode_ms
        }

        fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), String> {
            let mut cfg_copy = self.encode_config;
            let frame_bits = self
                .settings
                .vbv_bits(bitrate_bps, self.init_params.frameRateNum);
            cfg_copy.rcParams.averageBitRate = bitrate_bps;
            cfg_copy.rcParams.maxBitRate = self.settings.max_bitrate(bitrate_bps);
            cfg_copy.rcParams.vbvBufferSize = frame_bits;
            cfg_copy.rcParams.vbvInitialDelay = frame_bits;
            unsafe { apply_nvenc_plan(&mut cfg_copy, self.codec, &self.plan) };
            let mut reinit = self.init_params;
            reinit.encodeConfig = &mut cfg_copy;

            let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { core::mem::zeroed() };
            reconf.version = self.rung.reconfigure_params_ver();
            reconf.reInitEncodeParams = reinit;
            // Zero-reset, zero-IDR: congestion control changes the bitrate on
            // the very next P-frame without a keyframe spike or a stall.
            reconf.set_resetEncoder(0);
            reconf.set_forceIDR(0);

            let s = unsafe { (self.api.nvEncReconfigureEncoder)(self.encoder, &mut reconf) };
            if s != NVENCSTATUS::SUCCESS {
                return Err(format!("nvEncReconfigureEncoder failed: {s:?}"));
            }
            self.encode_config = cfg_copy;
            self.plan.rate_control.vbv_bits = frame_bits;
            Ok(())
        }

        fn force_keyframe(&mut self) {
            self.force_idr_next = true;
            self.force_ir_frames = 0;
        }

        fn reconfigure_bitrate(&mut self, bitrate_bps: u32, fps: u32) -> Result<(), String> {
            let previous_fps = self.init_params.frameRateNum;
            self.init_params.frameRateNum = fps.max(1);
            let result = self.set_bitrate_bps(bitrate_bps);
            if result.is_err() {
                self.init_params.frameRateNum = previous_fps;
            }
            result
        }

        fn request_intra_refresh(&mut self, frames: u32) {
            if self.intra_refresh.enabled {
                self.force_ir_frames = frames.clamp(1, self.intra_refresh.refresh_frames);
            } else {
                self.force_keyframe();
            }
        }

        /// Idle refinement: drop the QP floor for a couple of frames so a
        /// static desktop snaps to near-lossless clarity, then restore the
        /// streaming plan automatically.
        fn request_refinement(&mut self, frames: u32) {
            self.refine_frames = frames.max(1);
            let previous = self.plan.clone();
            self.plan.begin_refinement();
            let bps = self.encode_config.rcParams.averageBitRate;
            if self.set_bitrate_bps(bps).is_err() {
                self.plan = previous;
                self.refine_frames = 0;
            }
        }

        fn encode(
            &mut self,
            capturer: &mut DxgiCapturer,
            timestamp_us: u64,
        ) -> Result<Option<EncodedAccessUnit>, String> {
            let t_encode_start = Instant::now();

            if !self.is_zero_copy {
                capturer.read_bgra_into(&mut self.cpu_staging)?;
                unsafe {
                    self.nv_context.UpdateSubresource(
                        &self.input_texture,
                        0,
                        None,
                        self.cpu_staging.as_ptr().cast(),
                        self.width * 4,
                        0,
                    );
                }
            }

            self.yuv_converter.convert(
                &self.nv_context,
                &self.input_texture,
                self.width,
                self.height,
            )?;

            let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { core::mem::zeroed() };
            map.version = self.rung.map_input_resource_ver();
            map.registeredResource = self.registered_res;
            let s = unsafe { (self.api.nvEncMapInputResource)(self.encoder, &mut map) };
            if s != NVENCSTATUS::SUCCESS {
                return Err(format!("nvEncMapInputResource failed: {s:?}"));
            }

            let mut pic: NV_ENC_PIC_PARAMS = unsafe { core::mem::zeroed() };
            pic.version = self.rung.pic_params_ver();
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = self.width;
            pic.frameIdx = self.frame_idx;
            self.frame_idx = self.frame_idx.wrapping_add(1);
            pic.inputTimeStamp = timestamp_us;
            pic.inputBuffer = map.mappedResource;
            pic.outputBitstream = self.bitstream_buf;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.pictureStruct = NV_ENC_PIC_STRUCT::FRAME;

            if self.force_idr_next {
                // AV1 has no SPS/PPS pair to emit: `repeatSeqHdr` already puts
                // the sequence header OBU in front of every key frame.
                pic.encodePicFlags = if self.codec == VideoCodec::Av1 {
                    NV_ENC_PIC_FLAG_FORCEIDR
                } else {
                    NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
                };
                self.force_idr_next = false;
            } else if self.force_ir_frames > 0 {
                let frames = core::mem::take(&mut self.force_ir_frames);
                match self.codec {
                    VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                        pic.codecPicParams
                            .hevcPicParams
                            .forceIntraRefreshWithFrameCnt = frames;
                    }
                    VideoCodec::H264 => {
                        pic.codecPicParams
                            .h264PicParams
                            .forceIntraRefreshWithFrameCnt = frames;
                    }
                    VideoCodec::Av1 => {
                        pic.codecPicParams
                            .av1PicParams
                            .forceIntraRefreshWithFrameCnt = frames;
                    }
                }
            }

            let enc_status = unsafe { (self.api.nvEncEncodePicture)(self.encoder, &mut pic) };
            let _ = unsafe { (self.api.nvEncUnmapInputResource)(self.encoder, map.mappedResource) };

            if enc_status == NVENCSTATUS::ERR_NEED_MORE_INPUT {
                return Ok(None);
            }
            if enc_status != NVENCSTATUS::SUCCESS {
                return Err(format!("nvEncEncodePicture failed: {enc_status:?}"));
            }

            let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { core::mem::zeroed() };
            lock.version = self.rung.lock_bitstream_ver();
            lock.outputBitstream = self.bitstream_buf;

            let lock_status = unsafe { (self.api.nvEncLockBitstream)(self.encoder, &mut lock) };
            if lock_status != NVENCSTATUS::SUCCESS {
                return Err(format!("nvEncLockBitstream failed: {lock_status:?}"));
            }

            let encode_us = t_encode_start
                .elapsed()
                .as_micros()
                .min(u128::from(u32::MAX)) as u32;
            let slice = unsafe {
                core::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            };
            let payload = slice.to_vec();
            let pic_is_key =
                lock.pictureType == NV_ENC_PIC_TYPE::IDR || is_keyframe(self.codec, &payload);

            let _ = unsafe { (self.api.nvEncUnlockBitstream)(self.encoder, self.bitstream_buf) };

            // Record exact Parsec-style wall-clock encode duration (microseconds -> EMA ms)
            self.last_encode_ms = crate::capture::record_encode_latency_us(encode_us);

            if self.refine_frames > 0 {
                self.refine_frames -= 1;
                if self.refine_frames == 0 {
                    self.plan.end_refinement();
                    self.plan.rate_control.min_qp_intra = self.settings.min_qp.min(51);
                    self.plan.rate_control.min_qp_inter_p = self.settings.min_qp.min(51);
                    self.plan.rate_control.aq_strength = self.settings.aq_strength;
                    let bps = self.encode_config.rcParams.averageBitRate;
                    let _ = self.set_bitrate_bps(bps);
                }
            }

            Ok(Some(EncodedAccessUnit {
                codec: self.codec,
                is_keyframe: pic_is_key,
                payload,
                host_encode_us: encode_us,
            }))
        }
    }

    static MF_INIT: Once = Once::new();

    fn ensure_mf_started() {
        MF_INIT.call_once(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        });
    }

    fn make_variant_bool(val: bool) -> VARIANT {
        VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: core::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_BOOL,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 {
                        boolVal: val.into(),
                    },
                }),
            },
        }
    }

    fn make_variant_u32(val: u32) -> VARIANT {
        VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: core::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_UI4,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { ulVal: val },
                }),
            },
        }
    }

    /// Best-effort `ICodecAPI` write for a `VT_UI4` value.
    ///
    /// Vendor MFTs advertise capabilities they do not implement, so a rejection
    /// here is information, not an error: the caller decides what is essential.
    fn set_codec_u32(api: &ICodecAPI, guid: &GUID, value: u32) -> bool {
        let variant = make_variant_u32(value);
        unsafe { api.SetValue(guid, &variant) }.is_ok()
    }

    fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | (lo as u64)
    }

    pub struct MftVideoEncoder {
        transform: IMFTransform,
        codec_api: Option<ICodecAPI>,
        codec: VideoCodec,
        pipeline_label: String,
        is_hardware: bool,
        width: u32,
        height: u32,
        fps: u16,
        frame_idx: u64,
        force_keyframe_next: bool,
        /// Quality-leaning encoder effort, re-applied on every bitrate change.
        quality_first: bool,
        sequence_header: Vec<u8>,
        cpu_bgra: Vec<u8>,
        nv12_buf: Vec<u8>,
        provides_samples: bool,
        last_encode_ms: f32,
    }

    unsafe impl Send for MftVideoEncoder {}

    impl MftVideoEncoder {
        pub fn new(
            width: u32,
            height: u32,
            fps: u16,
            bitrate_bps: u32,
            codec: VideoCodec,
            software_only: bool,
            quality_first: bool,
        ) -> Result<Self, String> {
            ensure_mf_started();
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            if matches!(codec, VideoCodec::HevcMain10 | VideoCodec::Hevc444) {
                return Err("Main10 requires the NVENC path".to_string());
            }
            let out_subtype = match codec {
                VideoCodec::H264 => MFVideoFormat_H264,
                VideoCodec::Hevc | VideoCodec::HevcMain10 | VideoCodec::Hevc444 => {
                    MFVideoFormat_HEVC
                }
                VideoCodec::Av1 => MFVideoFormat_AV1,
            };
            let output_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: out_subtype,
            };

            let flags = if software_only {
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
            } else {
                MFT_ENUM_FLAG_HARDWARE
                    | MFT_ENUM_FLAG_ASYNCMFT
                    | MFT_ENUM_FLAG_SYNCMFT
                    | MFT_ENUM_FLAG_SORTANDFILTER
            };

            let mut activates_ptr: *mut Option<IMFActivate> = core::ptr::null_mut();
            let mut count: u32 = 0;
            unsafe {
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    None,
                    Some(&output_info),
                    &mut activates_ptr,
                    &mut count,
                )
            }
            .map_err(|e| format!("MFTEnumEx failed: {e}"))?;

            if count == 0 || activates_ptr.is_null() {
                return Err(format!("no Media Foundation encoder found for {codec:?}"));
            }

            let mut chosen: Option<(IMFTransform, bool, bool)> = None;
            let activates_slice =
                unsafe { core::slice::from_raw_parts(activates_ptr, count as usize) };
            for maybe_act in activates_slice {
                let Some(act) = maybe_act else {
                    continue;
                };
                let Ok(mft) = (unsafe { act.ActivateObject::<IMFTransform>() }) else {
                    continue;
                };
                // The low-latency attribute belongs to the transform and is
                // only read before the media types are negotiated, so it is
                // requested here rather than inside `configure_mft`.
                let low_latency_attribute = unsafe { mft.GetAttributes() }
                    .map(|attributes| unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) }.is_ok())
                    .unwrap_or(false);
                if Self::configure_mft(&mft, width, height, fps, bitrate_bps, out_subtype).is_ok() {
                    chosen = Some((mft, !software_only, low_latency_attribute));
                    break;
                }
                let _ = unsafe { act.ShutdownObject() };
            }

            unsafe {
                CoTaskMemFree(Some(activates_ptr.cast()));
            }

            let Some((transform, is_hardware, low_latency_attribute)) = chosen else {
                return Err(format!(
                    "all MFT encoder candidates failed setup for {codec:?}"
                ));
            };

            let codec_api: Option<ICodecAPI> = transform.cast().ok();
            let mut latency = crate::encoder_quality::MftLatencyReport {
                hardware: is_hardware,
                low_latency_attribute,
                ..crate::encoder_quality::MftLatencyReport::default()
            };
            if let Some(ref api) = codec_api {
                let v_low = make_variant_bool(true);
                latency.low_latency_mode =
                    unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &v_low) }.is_ok();
                if is_hardware {
                    // Vendor hardware encoders (AMD AMF, Intel Quick Sync, some
                    // Media Foundation filters) enable B-frames by default, and
                    // every B-frame is a reorder delay on top of the capture
                    // queue. The software fallback is deliberately not asked:
                    // it is short of bits, not of a frame time, and it has no
                    // such delay to remove in the first place.
                    latency.no_b_frames =
                        set_codec_u32(api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
                }
                latency.rate_control = Self::apply_rate_control(
                    api,
                    crate::encoder_quality::MftRateControl::for_stream(
                        bitrate_bps,
                        fps,
                        quality_first,
                    ),
                    codec,
                )?;
            }

            let sequence_header = Self::read_sequence_header(&transform);
            let out_info = unsafe { transform.GetOutputStreamInfo(0) }
                .map_err(|e| format!("GetOutputStreamInfo failed: {e}"))?;
            let provides_samples = (out_info.dwFlags & 0x100) != 0;

            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }

            let codec_label = match codec {
                VideoCodec::Hevc => "H.265",
                VideoCodec::HevcMain10 => "H.265 10-bit",
                VideoCodec::Hevc444 => "H.265",
                VideoCodec::H264 => "H.264",
                VideoCodec::Av1 => "AV1",
            };
            let mode_label = if software_only {
                "Software"
            } else {
                "Hardware"
            };
            let pipeline_label = format!(
                "{mode_label} {codec_label} {width}x{height} 4:2:0 Full Range (Opus Audio) - {}",
                latency.label()
            );
            if latency.is_complete() {
                tracing::info!(
                    pipeline = %pipeline_label,
                    ?codec,
                    hardware = is_hardware,
                    "MFT accepted the full low-latency pipeline"
                );
            } else {
                tracing::warn!(
                    pipeline = %pipeline_label,
                    rejected = ?latency.rejected(),
                    ?codec,
                    hardware = is_hardware,
                    "MFT refused part of the low-latency pipeline"
                );
            }

            Ok(Self {
                transform,
                codec_api,
                codec,
                pipeline_label,
                is_hardware,
                width,
                height,
                fps,
                frame_idx: 0,
                force_keyframe_next: true,
                quality_first,
                sequence_header,
                cpu_bgra: Vec::new(),
                nv12_buf: Vec::new(),
                provides_samples,
                last_encode_ms: 0.0,
            })
        }

        /// Push the streaming rate-control plan through `ICodecAPI`.
        ///
        /// Only the mean bitrate is treated as essential: an encoder that
        /// cannot hold an average rate cannot stream, and the caller must find
        /// out. Everything else is best-effort, because the same GUIDs are
        /// advertised by encoders that quietly ignore them.
        /// Returns whether a rate-control mode was accepted (the requested
        /// peak-constrained VBR, or the CBR fallback).
        fn apply_rate_control(
            api: &ICodecAPI,
            plan: crate::encoder_quality::MftRateControl,
            codec: VideoCodec,
        ) -> Result<bool, String> {
            // Order matters: the rate-control mode decides how the bitrate
            // values are validated.
            let mut accepted = set_codec_u32(api, &CODECAPI_AVEncCommonRateControlMode, plan.mode);
            if !accepted {
                // A CBR-only encoder still produces a usable stream, but the
                // log should say why the picture is softer than it needs to be.
                if set_codec_u32(
                    api,
                    &CODECAPI_AVEncCommonRateControlMode,
                    crate::encoder_quality::RC_CBR,
                ) {
                    warn!("MFT rejected peak-constrained VBR, falling back to CBR");
                    accepted = true;
                }
            }

            // Encoder effort and GOP are set once, before streaming starts.
            // Re-pushing them mid-stream makes some vendor MFTs reconfigure and
            // stall a frame; only the rate values move after this.
            let _ = set_codec_u32(
                api,
                &CODECAPI_AVEncCommonQualityVsSpeed,
                plan.quality_vs_speed,
            );
            let _ = set_codec_u32(api, &CODECAPI_AVEncMPVGOPSize, plan.gop_length);
            if codec == VideoCodec::H264 {
                // CABAC is worth roughly 10% bitrate at equal quality and the
                // MF High-profile encoders all ship it.
                let cabac = make_variant_bool(true);
                let _ = unsafe { api.SetValue(&CODECAPI_AVEncH264CABACEnable, &cabac) };
            }
            Self::apply_bitrate(api, plan)?;
            Ok(accepted)
        }

        /// Peak, buffer and average: the difference between "compressed" and
        /// "crisp" on every host that is not running NVENC.
        fn apply_bitrate(
            api: &ICodecAPI,
            plan: crate::encoder_quality::MftRateControl,
        ) -> Result<(), String> {
            let mean = make_variant_u32(plan.mean_bitrate_bps);
            unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &mean) }
                .map_err(|e| format!("ICodecAPI bitrate update failed: {e}"))?;
            let _ = set_codec_u32(api, &CODECAPI_AVEncCommonMaxBitRate, plan.max_bitrate_bps);
            let _ = set_codec_u32(api, &CODECAPI_AVEncCommonBufferSize, plan.buffer_size_bits);
            Ok(())
        }

        fn configure_mft(
            mft: &IMFTransform,
            width: u32,
            height: u32,
            fps: u16,
            bitrate_bps: u32,
            out_subtype: GUID,
        ) -> Result<(), String> {
            unsafe {
                let out_type = MFCreateMediaType().map_err(|e| e.to_string())?;
                out_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(|e| e.to_string())?;
                out_type
                    .SetGUID(&MF_MT_SUBTYPE, &out_subtype)
                    .map_err(|e| e.to_string())?;
                out_type
                    .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
                    .map_err(|e| e.to_string())?;
                out_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(|e| e.to_string())?;
                out_type
                    .SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))
                    .map_err(|e| e.to_string())?;
                out_type
                    .SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1) as u32, 1))
                    .map_err(|e| e.to_string())?;
                Self::set_bt709_full_range(&out_type)?;
                mft.SetOutputType(0, &out_type, 0)
                    .map_err(|e| format!("SetOutputType: {e}"))?;

                let in_type = MFCreateMediaType().map_err(|e| e.to_string())?;
                in_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(|e| e.to_string())?;
                in_type
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                    .map_err(|e| e.to_string())?;
                in_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(|e| e.to_string())?;
                in_type
                    .SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))
                    .map_err(|e| e.to_string())?;
                in_type
                    .SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1) as u32, 1))
                    .map_err(|e| e.to_string())?;
                Self::set_bt709_full_range(&in_type)?;
                mft.SetInputType(0, &in_type, 0)
                    .map_err(|e| format!("SetInputType: {e}"))?;
            }
            Ok(())
        }

        fn set_bt709_full_range(
            media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        ) -> Result<(), String> {
            for (attribute, value) in [
                (MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0),
                (MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0),
                (MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0),
                (MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0),
            ] {
                unsafe { media_type.SetUINT32(&attribute, value as u32) }
                    .map_err(|err| format!("Set BT.709 media type: {err}"))?;
            }
            Ok(())
        }

        fn read_sequence_header(mft: &IMFTransform) -> Vec<u8> {
            let Ok(out_type) = (unsafe { mft.GetOutputCurrentType(0) }) else {
                return Vec::new();
            };
            let Ok(len) = (unsafe { out_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
                return Vec::new();
            };
            if len == 0 {
                return Vec::new();
            }
            let mut buf = vec![0u8; len as usize];
            if unsafe { out_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) }.is_ok() {
                buf
            } else {
                Vec::new()
            }
        }
    }

    impl Drop for MftVideoEncoder {
        fn drop(&mut self) {
            unsafe {
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
        }
    }

    impl WinVideoEncoder for MftVideoEncoder {
        fn codec(&self) -> VideoCodec {
            self.codec
        }

        fn pipeline_label(&self) -> &str {
            &self.pipeline_label
        }

        fn hardware_load_hint(&self) -> bool {
            self.is_hardware
        }

        fn last_encode_latency_ms(&self) -> f32 {
            self.last_encode_ms
        }

        fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), String> {
            if let Some(ref api) = self.codec_api {
                // Only the rate values move: the mode, effort and GOP stay as
                // negotiated, so a congestion dip can never quietly turn the
                // encoder into a thriftier (softer) one. The peak and buffer
                // are rebased with the average — a peak left at the old bitrate
                // stops constraining anything, and a stale buffer turns a drop
                // into a burst.
                Self::apply_bitrate(
                    api,
                    crate::encoder_quality::MftRateControl::for_stream(
                        bitrate_bps,
                        self.fps,
                        self.quality_first,
                    ),
                )?;
            }
            Ok(())
        }

        fn force_keyframe(&mut self) {
            self.force_keyframe_next = true;
        }

        fn encode(
            &mut self,
            capturer: &mut DxgiCapturer,
            _timestamp_us: u64,
        ) -> Result<Option<EncodedAccessUnit>, String> {
            let t_encode_start = Instant::now();
            capturer.read_bgra_into(&mut self.cpu_bgra)?;
            bgra_to_nv12(
                &self.cpu_bgra,
                self.width as usize,
                self.height as usize,
                &mut self.nv12_buf,
            );

            if self.force_keyframe_next
                && let Some(ref api) = self.codec_api
            {
                let v = make_variant_u32(1);
                let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v) };
                self.force_keyframe_next = false;
            }

            let sample: IMFSample =
                unsafe { MFCreateSample() }.map_err(|e| format!("MFCreateSample failed: {e}"))?;
            let buffer: IMFMediaBuffer =
                unsafe { MFCreateMemoryBuffer(self.nv12_buf.len() as u32) }
                    .map_err(|e| format!("MFCreateMemoryBuffer failed: {e}"))?;

            unsafe {
                let mut ptr: *mut u8 = core::ptr::null_mut();
                buffer
                    .Lock(&mut ptr, None, None)
                    .map_err(|e| format!("buffer Lock failed: {e}"))?;
                core::ptr::copy_nonoverlapping(self.nv12_buf.as_ptr(), ptr, self.nv12_buf.len());
                let _ = buffer.Unlock();
                let _ = buffer.SetCurrentLength(self.nv12_buf.len() as u32);
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| format!("AddBuffer failed: {e}"))?;
                let dur_100ns = 10_000_000i64 / (self.fps.max(1) as i64);
                let _ = sample.SetSampleTime(self.frame_idx as i64 * dur_100ns);
                let _ = sample.SetSampleDuration(dur_100ns);
            }
            self.frame_idx += 1;

            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .map_err(|e| format!("ProcessInput failed: {e}"))?;

            let mut out_sample_holder: Option<IMFSample> = if !self.provides_samples {
                let out_s = unsafe { MFCreateSample() }
                    .map_err(|e| format!("out MFCreateSample failed: {e}"))?;
                let out_b = unsafe { MFCreateMemoryBuffer(self.width * self.height * 2) }
                    .map_err(|e| format!("out MFCreateMemoryBuffer failed: {e}"))?;
                unsafe {
                    let _ = out_s.AddBuffer(&out_b);
                }
                Some(out_s)
            } else {
                None
            };

            let mut out_bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: core::mem::ManuallyDrop::new(out_sample_holder.take()),
                dwStatus: 0,
                pEvents: core::mem::ManuallyDrop::new(None),
            }];
            let mut status: u32 = 0;

            let proc_res = unsafe { self.transform.ProcessOutput(0, &mut out_bufs, &mut status) };
            let [out_buf] = out_bufs;
            let produced = core::mem::ManuallyDrop::into_inner(out_buf.pSample);
            let _events = core::mem::ManuallyDrop::into_inner(out_buf.pEvents);

            match proc_res {
                Ok(()) => {
                    let Some(s) = produced else {
                        return Ok(None);
                    };
                    let contig = unsafe { s.ConvertToContiguousBuffer() }
                        .map_err(|e| format!("ConvertToContiguousBuffer failed: {e}"))?;
                    let mut out_ptr: *mut u8 = core::ptr::null_mut();
                    let mut cur_len: u32 = 0;
                    unsafe {
                        contig
                            .Lock(&mut out_ptr, None, Some(&mut cur_len))
                            .map_err(|e| format!("output Lock failed: {e}"))?;
                    }
                    let bytes = if cur_len == 0 {
                        &[]
                    } else {
                        unsafe { core::slice::from_raw_parts(out_ptr, cur_len as usize) }
                    };
                    let is_key = is_keyframe(self.codec, bytes);
                    let payload = if is_key && !bytes.starts_with(&self.sequence_header) {
                        let mut prefixed =
                            Vec::with_capacity(self.sequence_header.len() + bytes.len());
                        prefixed.extend_from_slice(&self.sequence_header);
                        prefixed.extend_from_slice(bytes);
                        prefixed
                    } else {
                        bytes.to_vec()
                    };
                    unsafe {
                        let _ = contig.Unlock();
                    }

                    let encode_us = t_encode_start.elapsed().as_micros() as u32;
                    self.last_encode_ms = crate::capture::record_encode_latency_us(encode_us);

                    Ok(Some(EncodedAccessUnit {
                        codec: self.codec,
                        is_keyframe: is_key,
                        payload,
                        host_encode_us: encode_us,
                    }))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) => Err(format!("ProcessOutput failed: {e}")),
            }
        }
    }
}
#[cfg(test)]
mod av1_keyframe_tests {
    use super::{is_av1_keyframe, is_keyframe};
    use crate::protocol::VideoCodec;

    /// One OBU with a one-byte size field.
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_key_frame_obu_is_detected() {
        let mut unit = obu(2, &[]); // temporal delimiter
        unit.extend(obu(1, &[0x0A, 0x0B])); // sequence header
        unit.extend(obu(6, &[0x10])); // show_existing=0, frame_type=KEY_FRAME
        assert!(is_av1_keyframe(&unit));
        assert!(is_keyframe(VideoCodec::Av1, &unit));
    }

    #[test]
    fn an_inter_frame_is_not_a_keyframe() {
        let mut unit = obu(2, &[]);
        unit.extend(obu(6, &[0x30])); // frame_type = INTER
        assert!(!is_av1_keyframe(&unit));
        assert!(!is_keyframe(VideoCodec::Av1, &unit));
    }

    #[test]
    fn a_show_existing_frame_is_not_a_keyframe() {
        // show_existing_frame reuses an earlier picture; it is not a refresh.
        assert!(!is_av1_keyframe(&obu(6, &[0x80])));
    }

    #[test]
    fn multi_byte_obu_sizes_are_skipped() {
        let mut unit = obu(2, &[]);
        // Sequence header with a 200-byte payload: size is leb128 (0xC8 0x01).
        unit.push((1 << 3) | 0x02);
        unit.extend_from_slice(&[0xC8, 0x01]);
        unit.extend_from_slice(&[0u8; 200]);
        unit.extend(obu(6, &[0x10]));
        assert!(is_av1_keyframe(&unit));
    }

    #[test]
    fn h26x_detection_still_works_through_the_dispatcher() {
        // Annex-B H.264: a NAL type 5 (IDR) is a key frame, type 1 is not.
        assert!(is_keyframe(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 0x88]));
        assert!(!is_keyframe(VideoCodec::H264, &[0, 0, 0, 1, 0x41, 0x9A]));
        // An AV1 byte stream fed to the H.26x scanner must not crash or lie.
        assert!(!is_av1_keyframe(&[0x00]));
    }
}
