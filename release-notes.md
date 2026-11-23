## VmaX 6.0.4

- Best Connection defaults to 1440p HEVC 4:2:0 at 60 FPS with a 24 Mbps cap. The Windows host uses GPU-only NVIDIA NVENC in this mode. Constant FPS and encoder tuning remain configurable. Legacy protocols still support Media Foundation hosting.
- Raw UDP drains packets on a dedicated receive thread, repairs single-packet loss locally, and sends deltas in shorter paced bursts on healthy LAN connections.
- Android uses asynchronous MediaCodec callbacks and presents directly to a SurfaceView. Windows native playback uses matching BT.709 range metadata. WebView fallback continues accepting frames while the decoder works.
- The telemetry overlay reports estimated one-way network time, bitrate, frame and packet drops, and FEC recoveries.

### Downloads

- `VmaX.exe` for Windows
- `VmaX-Android-arm64.apk` for Android ARM64
- `VmaX-Relay-Linux-x86_64.tar.gz` for a self-hosted Linux signaling relay

Android may require a clean install if the APK signing certificate differs from your installed version. Uninstalling removes local app data.

See the [regional relay guide](https://github.com/VibeXme/VmaX/blob/v6.0.4/deploy/server/README.md) for self-hosting.
