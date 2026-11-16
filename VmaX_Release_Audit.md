# VmaX engineering audit and release validation

## Architecture found

| Layer | Current implementation |
| --- | --- |
| App | React UI in `src/`, Tauri commands in `src-tauri/src/commands.rs`, Android activity and plugins under `src-tauri/gen/android/` |
| Windows capture | DXGI Desktop Duplication and D3D11 in `capture.rs`. NVENC can use the GPU texture path. Scaling and fallback paths can require additional copies. |
| Encoding | `encoder.rs` tries NVENC for supported H.264 or HEVC, then Windows Media Foundation hardware and software transforms. NVENC already uses low latency tuning, no B-frames, and a small VBV. |
| Decode and presentation | Binary Tauri channel, WebCodecs and canvas in `StreamViewer.tsx`. Both client platforms currently use this path. |
| Audio | CPAL/WASAPI loopback capture on Windows, PCM datagrams, optional RNNoise and CPAL playback with bounded buffering in `audio.rs`. The protocol defines an Opus identifier, but the live pipeline is PCM. |
| Connectivity | `signaling.rs` handles room discovery and WebSocket signaling. `stun.rs` gathers public candidates. `network.rs` probes UDP and negotiates authenticated Quinn QUIC or relay fallback. |
| Relay | The standalone Rust signaling service forwards opaque QUIC packets over WebSockets. A temporary Cloudflare tunnel exposes the built-in server when the host has no configured public VmaX relay. |
| Packet formats | `protocol.rs` defines video fragmentation, XOR FEC, reassembly, audio, input and control messages. Video and audio are datagrams. Keyboard and control use reliable streams. |
| Input | Browser pointer and keyboard events plus native XInput or Android touch reports. Windows injects keys and pointer events and exposes a virtual Xbox controller through ViGEmBus. Session teardown releases held inputs. |
| Timing and diagnostics | Capture and playback use dedicated workers. `bitrate.rs` uses RTT, loss, frame drops and sender queue delay. The UI exposes connection path, RTT, video bitrate, FPS and decode time. |

## Changes made

This release keeps the existing native Windows capture and encoder stack. It changes how paths are prioritized, how old input and frames are handled, and what the UI reports.

## Network improvements

- LAN candidates receive a short head start. Public candidates then participate together before the two peers agree on direct QUIC or relay.
- The host can be provisioned with up to eight comma-separated `wss://` VmaX relay addresses via `VMAX_PUBLIC_RELAY_URLS` at runtime or build time. Internet hosting races health checks and uses the first reachable relay. No address entry is needed for each host or client.
- Without a configured reachable service, the current app-managed temporary tunnel remains the fallback. Direct connections do not send media through it. If direct UDP fails with a temporary tunnel, encrypted media can still travel over that tunnel. Deploying a public VmaX relay removes that Cloudflare dependency, but the current standalone relay still uses WebSockets for media forwarding.
- Path labels describe the selected endpoint. Private IP paths are labeled Direct LAN. A VPN can also use private addresses, so this classification does not establish physical proximity.

## Video improvements

- Balanced defaults to 1080p60 at a 20 Mbps ceiling with 5% configured FEC. Low Latency, High Quality and Unstable Network presets remain selectable. Existing saved settings are preserved.
- The QUIC sender discards frames older than 65 ms measured from encode submission. After discarding a referenced frame it requests a fresh keyframe.
- WebCodecs recovery starts after a shorter queue spike or sustained backlog. Decoder references still require a keyframe to recover.
- HEVC decoder initialization reads the visible dimensions from the stream's sequence header. This fixed a WebCodecs output rectangle that cropped a 1080p stream to 720p during the Windows test.
- The encoder is unchanged. NVENC remains the preferred GPU path when available and Media Foundation remains the fallback.

## Audio improvements

- The host can turn computer audio off for the next session. Capture targets the default playback endpoint and never silently switches to the microphone.
- Apps using that playback endpoint are included. An app routing to a separate communications endpoint is not captured by this release. The audio transport still uses PCM.

## Input improvements

- Out-of-order absolute pointer positions and drag motion cannot move the host cursor back after a newer position. Reliable key and button transitions still run in order.
- Native XInput, touch gamepad and gamepad-mode Disconnect are preserved.

## Android improvements

- The app hides system bars during connected playback and restores them on exit or when settings open. Android can reveal bars temporarily with its edge gesture.
- The responsive touch overlay remains in place. Video still uses WebCodecs and canvas. A native MediaCodec Surface pipeline is not included.

## Windows improvements

- The single-file distribution is preserved. Windows supplies its installed WebView2 runtime and graphics drivers.
- WASAPI desktop capture, NVENC, Media Foundation and ViGEmBus integration are preserved.

## UI improvements

- The host and stream overlay show Direct LAN, Direct Internet or Relay with measured round-trip time. The dashboard no longer presents estimated NVENC load or invented VRAM usage as measurements.
- Stream controls remain compact and the new preset and audio toggle are under options/settings.

## Security fixes

- Production WebView content is restricted to bundled assets and Tauri IPC. Development has a separate localhost policy. [Tauri CSP documentation](https://v2.tauri.app/security/csp/)
- Room invite URLs and remote input contents are omitted from app packet logs. Existing certificate pinning and packet limits remain in place.

## Tests performed

- Formatted the modified Rust sources. The frontend built and TypeScript checks passed.
- Built both release packages. Android Gradle release lint and assembly passed. Verified the APK signature, ARM64 library and final frontend bundle. No handset was attached for playback testing.
- 26 core tests passed, including QUIC, raw UDP, reliable input, synthetic audio packets, stale video rejection and relay fallback. The standalone signaling server passed 10 tests.
- A public relay test joined an authenticated QUIC session and delivered an encrypted synthetic video frame through the tunnel. This did not measure sustained relay playback.
- The final Windows EXE opened from an empty directory with the final frontend. Two Windows instances on one PC rendered HEVC at 1920×1080 with about 55 FPS over a 12-second sample, 0.52 ms measured transport RTT and zero reported network reassembly drops. This is a loopback-scale network measurement, not end-to-end gameplay latency.
- In a separate test, the client reported HEVC unavailable and negotiated H.264. It rendered 1920×1080 at about 55 FPS. Gaming-mode disconnect passed with a touch joystick held in both tests. Touch was emulated.

## Tests requiring real devices

| Scenario | Procedure |
| --- | --- |
| Ethernet, Wi-Fi, hotspot | Join the same host from a second PC or phone. Check Direct LAN, 1080p60, RTT and drops for two minutes. |
| Different Wi-Fi, Wi-Fi to cellular | Use the full internet invite. Check whether Direct Internet or Relay is selected. Repeat with UDP blocked on a test network. |
| Loss, jitter, bandwidth reduction | Use a network emulator on an owned test path. Vary impairments separately and record bitrate recovery, frame drops and queue age. |
| Network interruption and handoff | Pause connectivity briefly, then switch Wi-Fi and cellular. Check that the app reports disconnection and that a new session can start. |
| Audio, Discord and separate output | Play game, browser and Discord audio on the default output, then move Discord to the communications output. Check mute, quality and latency. Separate endpoints need new capture support. |
| Input | Test held keys, rapid clicks, relative mouse movement and XInput/gamepad reports across a disconnect. Test physical Android controllers separately from touch. |
| H.264, HEVC and fallback | Test each codec on supported hardware and on a machine without NVENC. Confirm the actual negotiated codec and encoder. Native Android hardware decode needs a MediaCodec build and a physical device. |

## Remaining issues and recommended next improvements

1. Provision public relay infrastructure for a stable endpoint and global short-code lookup. Add a UDP/QUIC relay with authenticated room allocation, connection limits and encrypted forwarding. Keep HTTP/WebSockets for signaling and compatibility.
2. Add a native Android MediaCodec receiver and Surface lifecycle. Deliver access units from Rust to a bounded native queue without JavaScript serialization. Test H.264 and HEVC, hardware capability and low latency support on actual devices.
3. Capture Windows multimedia and communications endpoints by endpoint ID, deduplicate them, align clocks and mix to 48 kHz. Negotiate an Opus codec before replacing PCM so older clients can still join.
4. Add authenticated session resumption for brief outages and network changes. Saved peer reconnection exists, but active sessions do not yet migrate seamlessly between direct and relay paths.
5. Feed measured decoder backlog, jitter and delivery rate into adaptive bitrate and test adaptive FEC. Add capture/presentation latency, audio underrun and send rate metrics without estimating unavailable values.

## Important modified files

- `src-tauri/src/network.rs`, `internet.rs`, `commands.rs` and `input.rs` for routes, queue age, audio selection and input ordering.
- `src-tauri/src/audio.rs` for desktop capture selection.
- `src-tauri/src/lib.rs` and `gen/android/app/src/main/java/com/vmax/app/RemoteKeyboardPlugin.kt` for Android immersive playback.
- `src-tauri/tauri.conf.json` for content policy.
- `src/App.tsx`, `types.ts` and `config/defaults.ts` for connection UI and presets.
- `src/config/hevc.ts` and `src/components/StreamViewer.tsx` for HEVC geometry and frame recovery.
- `src/components/HostingDashboard.tsx`, `HardwareStats.tsx` and `SettingsModal.tsx` for measured diagnostics and audio controls.

## Smoothness and compatibility pass

Goal: a stream that *feels* native — a steady 60 FPS cadence on any device and
any working connection — instead of a high average frame rate with judder.

### Host cadence

- New `src-tauri/src/pacing.rs` (`FrameCadence`) replaces the fixed capture poll
  plus trailing sleep. Capture waits in `AcquireNextFrame` only for the remaining
  slot minus the measured encode cost, so it wakes when the desktop presents.
- The deadline grid is drift-free and rate-limited: a missed slot re-anchors the
  grid and is skipped rather than repaid as a burst. Covered by unit tests for
  drift, late slots, encode budgets and FPS changes.
- The encoder cost is an EMA that adapts the capture budget to the actual host.
- New capture-side backpressure: when the network worker's two-frame queue is
  full, the capture slot is skipped so the encoder never emits a frame that the
  sender must drop (dropping an encoded frame breaks references and forces an
  intra-refresh wave).
- Encoder budget guard: sustained encode overrun scales the capture/encode size
  in steps (floor 50%), restoring after headroom returns. It applies with a
  locked resolution too, because a stutter is never an acceptable trade.
- Constant FPS now holds the client's *presentable* cadence (the refresh rate the
  client reports), instead of encoding faster than the panel can show.

### Client presentation and recovery

- `FramePresenter` tracks presented-frame intervals, median interval and 95th
  percentile deviation, plus superseded frames, and surfaces them as
  **Frame pacing** in the stream details panel.
- WebCodecs decoder configs prefer hardware, set `optimizeForLatency`, and add
  `latencyMode: "realtime"` where supported (typed locally because the bundled
  DOM types do not declare it).
- Isolated frame loss no longer resets the decoder. The frame is concealed and
  the host's intra-refresh repairs it; a reset to keyframe-wait only happens
  after 12 consecutive gaps (~200 ms) or a sustained decoder backlog.
- The WebGL path flushes its command buffer after the draw instead of leaving
  submission to the next event-loop turn.

### Defaults

- Adaptive bitrate is the default (the configured value is the ceiling); fixed
  bitrate remains selectable for known-good links.
- Incomplete-frame recovery window raised from 18 ms to 32 ms. Complete frames
  are still delivered immediately, so this only adds recovery headroom on
  jittery paths.
- The Rust settings fallback moved from 4K/40 Mbps to 1080p60/20 Mbps so a
  session started without a settings payload negotiates a size every client
  class can carry.

### Verification

- `cargo test --lib`: 178 tests pass, including 6 new pacing tests.
- Frontend `tsc --noEmit` passes.
- Release artifacts rebuilt (see the build output above).
- Still requires physical-device validation: sustained two-minute playback on
  Ethernet, Wi-Fi and cellular, and the Android MediaCodec path on a handset.
  Frame pacing can now be measured directly in the stream details panel instead
  of inferred from FPS.

## Image quality pass

Goal: at the same bitrate, look as clean as a reference-quality cloud gaming
stream. The NVENC path was already configured explicitly; the other paths were
not.

### Media Foundation path (AMD, Intel, software)

Every host without an NVIDIA GPU runs the Media Foundation encoder cascade, which
was negotiating **plain CBR** with a vendor-default GOP. That is the classic
"liquid" look: the rate controller clips exactly the detailed frames and raises
QP on them, and a short GOP keeps spending bits on I-frames a progressive stream
does not need. New `src-tauri/src/encoder_quality.rs` decides the numbers in one
unit-tested place, and `encoder.rs` pushes them through `ICodecAPI`:

- `AVEncCommonRateControlMode` = peak-constrained VBR, with a CBR fallback (and a
  warning log) for encoders that reject it.
- `AVEncCommonMaxBitRate` = 2x the average, matching the NVENC peak, so a busy
  frame can borrow bits instead of being clipped.
- `AVEncCommonBufferSize` = two frame-times, the same buffer the NVENC plan uses.
- `AVEncCommonQualityVsSpeed` = 70, or 85 when detail pacing is selected.
- `AVEncMPVGOPSize` = two seconds, so keyframes stop costing a periodic bit spike.
- `AVEncH264CABACEnable` for H.264, worth roughly 10% bitrate at equal quality.

A congestion-driven bitrate change rebases the peak and buffer together, so a
temporary dip cannot leave a stale ceiling behind. The mode, effort and GOP are
set once before streaming and are never re-pushed: they must not be reduced on a
dip, and re-sending them mid-stream makes some vendor MFTs reconfigure and stall.

### Client sharpening

`FramePresenter` now scales the contrast-adaptive sharpening by the resample
ratio: full strength at 1:1 and when upscaling, tapering to about 80% at half
size. Sharpening is a reconstruction filter; applied at full strength to a
*downscaled* picture (a 1080p stream in a phone-sized box, a 4K host on a 1080p
screen) it puts ringing around every one-pixel text edge instead of recovering
detail.

### Verification

- `cargo test --lib` covers the new policy (VBR mode, 2x peak, buffer sizing, GOP
  bounds, detail effort, bitrate rebasing) alongside the existing suite.
- Windows and Android release artifacts rebuilt.
- Not verifiable here: the visual result on AMD and Intel hardware, and on a
  handset. The encoder label in the stream details panel reports which path was
  selected; the new policy is the only difference in the non-NVENC output.

