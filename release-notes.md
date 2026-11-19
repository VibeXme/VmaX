## VmaX 5.2.0

Ultra-low-latency P2P remote desktop and game streaming for Windows and Android.

### Highlights

- **Ultra Low Latency now the default** — The "ultra" preset (native 120 FPS, motion pacing, 16 ms packet window, zero FEC, ultra-low-latency congestion) ships as the out-of-the-box experience. Sub-30 ms glass-to-glass on LAN; <50 ms on clean Wi-Fi.
- **Native 120 FPS baseline** — Captures at the host's own display resolution ("Native") at a constant 120 FPS with a 60 Mbps adaptive ceiling. Congestion control trades bitrate before latency.
- **New "Native" resolution option** — Choose `Native` to keep the host's exact display size (up to 8K); the encoder never upscales.
- **One-frame repair window** — Jitter buffer reduced from 32 ms to 16 ms (one frame at 120 Hz) so incomplete frames are repaired without adding latency.
- **Stronger reconstruction sharpening** — Host sharpening 25, viewer sharpening 50 makes HUD text and weapon detail readable through 4:2:0.
- **192 kbps Opus audio** — Up from 128 kbps for clearer voice and game audio.
- **Windows power-throttling opt-out (EcoQoS)** — The process disables EcoQoS so capture/encode threads and the WebView pipeline keep full priority even when the window loses focus. **Fixes the "60 FPS when not in focus" issue.**
- **WebView2 background timer throttling disabled** — `--disable-background-timer-throttling` keeps the decode/present loop responsive.
- **Aggressive-then-backoff hole punching** — 5 ms intervals for the first 200 ms (40 attempts × 2 bursts), then relaxes to ICE-like cadence. Gets through strict NATs within a single RTT.
- **Duplicated LAN preset ID fixed** — Presets are now `ultra`, `smooth`, `latency`, `esports`, `quality`, `lan`, and `showcase`.

### Congestion Control & Network Improvements

- **UltraLowLatency congestion profile** — New `UltraLowLatency` sensitivity (3 ms/s threshold, 2 ms hold) reacts to queue buildup before latency is affected.
- **200 Hz feedback interval** — Delay-gradient sampling every 5 ms catches congestion within a single 120 FPS frame.
- **Faster recovery** — Lossless queue growth triggers 15% cut (down from 25%); recovery climbs at `current/12` per 100 ms tick.
- **2 MiB socket buffers** — Reduced from 4 MiB to minimize queueing latency while pacer smooths the wire.
- **100 ms keepalive** — Faster CGNAT mapping refresh and failure detection.
- **Faster MTU probing** — 60 s interval with 10 s black hole cooldown.

### Frame Pacing & Capture

- **Encode guard reduced to 250 µs** — Capture waits less, starts precisely when desktop presents.
- **Spin budget 300 µs** — Final spin loop burns less time, shorter capture-to-present path.
- **MAX_CAPTURE_WAIT 16 ms** — Idle desktop poll ceiling matches 120 FPS cadence.
- **High-res frame pacer** — Yields only final 500 µs, spin loop for precise deadline hitting.

### Jitter Buffer (Client)

- **MIN_HOLD 1 ms** — Clean LAN paths hold only 1 frame worth of delay.
- **MAX_HOLD 12 ms** — Tighter cap prevents bufferbloat on jittery paths.
- **p95 jitter estimator** — 0.5 ms base + 1.2× p95 jitter, adapts to path conditions.

### Fixes

- **Startup crash fixed.** The rustls crypto provider is now installed exactly once at startup. Both `ring` and `aws-lc-rs` are enabled by transitive dependencies, so rustls could not infer a default provider and panicked on the first TLS/signaling connection.
- **Android remote keyboard and immersive streaming** now work — the `set_remote_keyboard` and `set_stream_immersive` commands were registered with the Tauri invoke handler.
- Vite dev server aligned with the Tauri `devUrl` (port 1420).
- Removed duplicated tracing initialization and duplicated theme variables.
- The VmaX logo is bundled into the app and shown in the title bar and Quick Connect header.
- Settings modal header/footer layout cleaned up.

### Downloads

| File | Platform | Size |
| --- | --- | --- |
| `VmaX.exe` | Windows 10/11 (portable) | ~70 MB |
| `VmaX-Setup.exe` | Windows installer (NSIS) | ~52 MB |
| `VmaX-Android-arm64.apk` | Android 8.0+ (arm64-v8a) | ~15 MB |

### Notes

- The Windows build is self-contained; the installer also bundles the WebView2 bootstrapper and the VC++ redistributable.
- The APK is signed with a local development key for sideloading.
- Configuration migration: existing installs that never customized settings will transparently upgrade to the new native/120/low-latency baseline. Any deliberate change (or a preset the user picked) is left untouched.
- Verified against the current `main` branch.