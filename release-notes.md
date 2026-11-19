## VmaX 5.0.0

Ultra-low-latency P2P remote desktop and game streaming for Windows and Android.

### Fixes

- **Startup crash fixed.** The rustls crypto provider is now installed exactly
  once at startup. Both `ring` and `aws-lc-rs` are enabled by transitive
  dependencies, so rustls could not infer a default provider and panicked on the
  first TLS/signaling connection.
- **Android remote keyboard and immersive streaming** now work — the
  `set_remote_keyboard` and `set_stream_immersive` commands were registered with
  the Tauri invoke handler.
- Vite dev server aligned with the Tauri `devUrl` (port 1420).
- Removed duplicated tracing initialization and duplicated theme variables.
- The VmaX logo is bundled into the app and shown in the title bar and Quick
  Connect header.
- Settings modal header/footer layout cleaned up.

### Downloads

| File | Platform | Size |
| --- | --- | --- |
| `VmaX.exe` | Windows 10/11 (portable) | ~70 MB |
| `VmaX-Setup.exe` | Windows installer (NSIS) | ~52 MB |
| `VmaX-Android-arm64.apk` | Android 8.0+ (arm64-v8a) | ~15 MB |

### Notes

- The Windows build is self-contained; the installer also bundles the WebView2
  bootstrapper and the VC++ redistributable.
- The APK is signed with a local development key for sideloading.
- Verified against the current `main` branch.
