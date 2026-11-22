## VmaX 6.0.0

- Windows DXGI capture wakes on desktop updates and keeps sending frames during static scenes. Host and viewer logs report the timing of individual stages.
- The receive queue preserves pending reference frames and can replace them with a recovery keyframe. Hybrid connections request a keyframe promptly after a detected gap, at most once every 200 ms.
- Desktop mouse input is coalesced at a 200 Hz target. Hybrid relative-motion packets carry two prior samples and replay the final sample once so a lost packet can be recovered without applying movement twice.
- Windows native playback adds adjustable FidelityFX CAS. Supported single-viewer Raw UDP sessions can negotiate HEVC Main10.
- Android opens the lobby before attaching native video. MediaCodec uses low-latency tuning and reports decode and render timings separately.
- Windows hosts can opt in to matching the selected display to the stream resolution and viewer refresh limit. VmaX restores the previous mode when it still owns the mode change.
- Windows QUIC packet pacing uses a high-resolution timer. App-owned streaming UDP sockets request 256 KiB buffers in each direction. Direct Raw UDP and WebRTC apply best-effort DSCP marking.

### Downloads

- `VmaX.exe` for Windows
- `VmaX-Android-arm64.apk` for Android ARM64
- `VmaX-Relay-Linux-x86_64.tar.gz` for a self-hosted Linux signaling relay

Some earlier Android builds used a different signing certificate. If Android reports a signature mismatch, back up your settings before uninstalling the old build. Uninstalling removes local app data.

Per-slice NVENC transmission, native cursor-shape sync, and NVENC reference-picture invalidation are still pending. Sustained frame rate and end-to-end latency depend on the devices and network path. See the [regional relay guide](https://github.com/VibeXme/VmaX/blob/v6.0.0/deploy/server/README.md) for self-hosting.
