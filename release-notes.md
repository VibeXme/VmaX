## VmaX 5.1.0

Ultra-low-latency P2P remote desktop and game streaming for Windows and Android.

### Highlights

- **Native 120 FPS gaming baseline** — The default preset now captures at the host's own display resolution ("Native") at a constant 120 FPS with a 60 Mbps adaptive ceiling. Congestion control trades bitrate before latency, so a relayed session softens instead of gaining lag.
- **New "Native" resolution option** — Choose `Native` to keep the host's exact display size (up to 8K); the encoder never upscales.
- **One-frame repair window** — Jitter buffer reduced from 32 ms to 16 ms (one frame at 120 Hz) so incomplete frames are repaired without adding latency.
- **Stronger reconstruction sharpening** — Host sharpening 30 → viewer sharpening 55 makes HUD text and weapon detail readable through 4:2:0.
- **192 kbps Opus audio** — Up from 128 kbps for clearer voice and game audio.
- **Windows power-throttling opt-out** — The process disables EcoQoS so capture/encode threads and the WebView pipeline keep full priority even when the window loses focus.
- **Longer hole-punch budget** — The 1.5 s WAN cap is removed; direct NAT traversal now gets the full configured timeout instead of falling back to the TCP relay.
- **WebView2 background timer throttling disabled** — `--disable-background-timer-throttling` keeps the decode/present loop responsive.
- **Duplicated LAN preset ID fixed** — The old `lan` preset ID collision is resolved; presets are now `native`, `lan`, `latency`, `esports`, and `showcase`.

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