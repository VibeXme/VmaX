#![cfg(windows)]

use mini_nvcodec::guids::NV_ENC_PRESET_P3_GUID;
use std::time::{Duration, Instant};
use vmax_lib::{
    capture::{HighResFramePacer, set_constant_fps, win::DxgiCapturer},
    encoder::win::{NvencD3d11Encoder, WinVideoEncoder},
    nvenc::NvencSettings,
    protocol::VideoCodec,
};

#[test]
#[ignore = "requires a Windows desktop and a compatible NVIDIA encoder"]
fn dxgi_nvenc_survives_bitrate_changes_and_keyframe_requests() {
    set_constant_fps(true);
    let mut capture = DxgiCapturer::new(1920, 1080, None).expect("create DXGI capture");
    for codec in [
        VideoCodec::H264,
        VideoCodec::Hevc,
        VideoCodec::HevcMain10,
        VideoCodec::Hevc444,
    ] {
        let mut encoder = NvencD3d11Encoder::new(
            &capture,
            codec,
            60,
            30_000_000,
            NV_ENC_PRESET_P3_GUID,
            NvencSettings::default(),
        )
        .expect("create NVENC encoder");
        let setup = Instant::now();
        while !capture
            .capture_frame(50, true)
            .expect("capture initial frame")
        {
            assert!(
                setup.elapsed() < Duration::from_secs(2),
                "desktop did not produce a frame"
            );
        }
        let start = Instant::now();
        let mut pacer = HighResFramePacer::default();
        let mut keyframes = 0;
        for index in 0..480 {
            if index % 60 == 0 {
                let bitrate = if index % 120 == 0 {
                    25_000_000
                } else {
                    45_000_000
                };
                encoder
                    .set_bitrate_bps(bitrate)
                    .expect("live bitrate update");
                encoder.force_keyframe();
            }
            assert!(capture.capture_frame(0, false).expect("capture frame"));
            let unit = encoder
                .encode(&mut capture, start.elapsed().as_micros() as u64)
                .expect("encode frame")
                .expect("encoder must produce output");
            assert_eq!(unit.codec, codec);
            assert!(!unit.payload.is_empty());
            keyframes += usize::from(unit.is_keyframe);
            pacer.wait_next_frame(60);
        }
        let fps = 480.0 / start.elapsed().as_secs_f64();
        eprintln!("{codec:?}: {fps:.2} FPS, {keyframes} keyframes");
        assert!(keyframes >= 8, "encoder missed a requested keyframe");
        assert!(
            fps >= 55.0,
            "capture/encode missed the real-time budget: {fps:.2} FPS"
        );
    }
}
