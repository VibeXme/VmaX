## VmaX 6.0.1

- Best Connection keeps the selected FPS as the host target and reuses the last valid capture on static screens. Actual playback depends on the encoder, viewer, and link.
- Direct Raw UDP gives recovery keyframes time to finish at the current link rate. The sender retains shards during socket backpressure and keeps an in-flight keyframe through repeated recovery requests.
- WebRTC sizes media packets for the underlying carrier, preserves pending recovery keyframes, and sends keyframe requests over the reliable control channel alongside RTP feedback.
- The Best Connection bitrate display measures received video traffic during frame recovery.

### Downloads

- `VmaX.exe` for Windows
- `VmaX-Android-arm64.apk` for Android ARM64
- `VmaX-Relay-Linux-x86_64.tar.gz` for a self-hosted Linux signaling relay

Android may require a clean install if your existing build uses a different signing certificate. Uninstalling removes local app data.

See the [regional relay guide](https://github.com/VibeXme/VmaX/blob/v6.0.1/deploy/server/README.md) for self-hosting.
