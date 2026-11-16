# VmaX

VmaX streams games and desktops from a Windows host to Windows, Android, and Linux clients.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Build VmaX

Run `./build_vmax_release.ps1` from PowerShell to produce `VmaX.exe` (Windows x64) and `VmaX-Android-arm64.apk` in this directory. Use `-SkipAndroid` or `-SkipWindows` to build a single platform. Windows requires the Visual Studio C++ tools and Windows SDK. Android requires a JDK and Android SDK with the NDK and Build-Tools.

Distribute `VmaX.exe` by itself. It embeds the frontend, WebView2 loader, internet helper, and controller driver installer. The app extracts the helper or installer into its local data directory when requested. Windows still needs the installed WebView2 runtime and the host's graphics drivers. The Android build is a streaming client for Android 8.0 or later and uses a local development key when Gradle produces an unsigned release APK.

### Install on Fedora

Install the VmaX RPM with `sudo dnf install ./VmaX-*.rpm` from the directory containing the package. Launch VmaX from your app menu or run `vmax`.

To build the RPM from a source checkout on Fedora, install Rust and Node.js, then run `bash install.sh --deps --rpm`. The package appears in the project directory. Build on the oldest Fedora release you intend to support.

### Install on Linux from source

From the VmaX source directory on Linux, run:

```bash
bash install.sh --deps
```

The optional `--deps` flag installs development libraries with DNF on Fedora Workstation or with APT on Debian or Ubuntu releases that provide WebKitGTK 4.1. Install Rust and a Node.js version supported by the locked frontend first. On other distributions, install the equivalent libraries and run `bash install.sh`.

The script installs `~/.local/bin/vmax` and adds VmaX to your app menu. Hosting requires Windows. Linux stream playback depends on the distro's WebKitGTK exposing WebCodecs with H.264 or HEVC decoding.

Run `bash install.sh --deps --appimage` on Linux to build a portable executable instead. For example, on an x86-64 PC the output is `VmaX-x86_64.AppImage`. After copying the file to another Linux PC, run `chmod +x VmaX-x86_64.AppImage` and then `./VmaX-x86_64.AppImage`.

## Connect on the same Wi-Fi

Start hosting on Windows with the default signaling endpoint. On the client, paste the full invite link or enter the eight-character room code. The short code discovers the host's built-in signaling service on the local network. The full link also includes the signaling address and pins the host certificate.

If the short code cannot find the room, use the full invite link. Local discovery uses UDP port 8788, and the embedded signaling endpoint defaults to TCP port 8787. The streaming socket uses an ephemeral UDP port. Guest Wi-Fi with client isolation can prevent local devices from reaching one another.

### Discover hosts on your network

On the client, open the **Discover** tab next to the invite field and pick a host:

- **On this network** lists PCs on the same Wi-Fi that are currently hosting. The scan sends a broadcast query on UDP port 8788 and hosting PCs answer with their room code, LAN endpoint, and computer name. Replies come back as unicast, so the scan works on Android without extra permissions. Hosts are only visible while a joinable room is open, and approved-only or saved sessions are never listed.
- **On the relay** lists rooms currently open on the selected signaling endpoint — the built-in LAN relay or a public `wss://` VmaX relay. Pick a saved relay in Settings first for internet sessions.

Tap **Connect** to join directly; no code typing needed. Hosts found on the network can also be saved under **Settings → Saved Connections** once connected, then reused with auto-connect or approval-gated reconnects.

## Connect across different networks

Run the VmaX signaling and relay server on an internet-reachable machine. It builds separately from the desktop and mobile apps. The gaming PC and client both connect outbound, so the gaming PC does not need router port forwarding.

### Deploy the relay with Docker Compose

On a public Linux server with Docker Compose, copy the project, point a DNS hostname at the server, and allow inbound TCP ports 80 and 443 on the server. From the project root, substitute your own hostname:

```sh
export VMAX_RELAY_DOMAIN=relay.example.com
docker compose -f signaling-server/compose.yaml up -d --build
```

Caddy provisions TLS for that hostname and proxies WebSockets to the relay on a private container network. Test the public endpoint from a different network before sending invites:

```sh
cargo run --release --locked --manifest-path signaling-server/Cargo.toml -- --check wss://relay.example.com
```

The check registers a temporary room, joins it, and verifies relay packet forwarding in both directions. It does not start a desktop stream. Then test a real join with the phone on cellular and Wi-Fi turned off.

### Use a public cloud relay

Run the Docker Compose relay on an always-on VM, such as Google Compute Engine, in a region that gives both devices a short route. Assign a public DNS name, allow inbound TCP 443 and TCP 80 for the included TLS proxy, then enter the resulting `wss://` address in VmaX. **Test media relay** checks room creation and packet forwarding in both directions from the current device. Run it from both devices to compare their paths to the relay.

The relay forwards the same encrypted QUIC stream, including the desktop image, video, and Opus audio. The Google STUN endpoint in VmaX assists direct connection setup. Media forwarding requires an endpoint running the VmaX relay protocol.

### Run the relay behind an existing TLS proxy

```sh
cargo run --release --locked --manifest-path signaling-server/Cargo.toml -- 127.0.0.1:8787
```

Put the service behind a TLS reverse proxy with WebSocket support. For example, a Caddy configuration for a domain pointed at the server is

```caddyfile
stream.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

### Start and join an internet session

Set the host's Signaling Endpoint to `wss://stream.example.com` before starting the room. Share the full invite link, or set the same endpoint on the client and enter the short code. Use QUIC for automatic fallback. VmaX probes available IPv4 and IPv6 routes on the media socket and tunnels encrypted QUIC through the relay when the direct path fails. Raw UDP requires a direct route.

When both peers support two-packet forward error correction, direct QUIC can recover up to two lost video datagrams per protected group at the same configured parity packet rate. Set the FEC percentage above zero to enable recovery. Older clients and relayed connections use single-packet recovery when FEC is enabled.

If either connection is behind carrier-grade NAT or direct connections keep failing, turn on **Always use the relay** in the host lobby after entering the public `wss://` endpoint. Relay-only hosting uses QUIC, skips the direct UDP check on both devices, and works with full invites and approved saved devices. Both devices need outbound access to the relay, typically on TCP port 443. Choose a relay close to both players to reduce the extra network delay. The expanded stream overlay shows whether the connection is direct or relayed when stats are enabled.

Automatic joins give local routes a head start and compare direct replies before falling back to a relay. The stream overlay shows measured network round-trip time and highlights values at or under the 30 ms target. This target excludes capture, encoding, decoding and display latency. Routes above the target can still connect, and the sender reduces how much video it queues on slower paths.

**Start Hosting** prepares one invite for local and internet clients. Add VmaX-compatible WSS addresses under **Host options → Manage relay addresses**, or set the comma-separated `VMAX_PUBLIC_RELAY_URLS` environment variable. The host probes up to eight distinct endpoints concurrently and uses the reachable relay with the lowest median response time measured from the host. If none responds, VmaX tries its managed tunnel and then falls back to a local-only invite.

To use Tailscale Funnel as another provider, run the VmaX signaling server on the relay machine bound to `127.0.0.1:8787`. Enable Funnel for that machine and run `tailscale funnel 8787`. Save the public address printed by Funnel in VmaX with the `wss://` scheme and verify it with the relay check above. Funnel requires an authorized tailnet and has bandwidth limits. [Tailscale Funnel documentation](https://tailscale.com/docs/features/tailscale-funnel)

Choose **Monitor to share** before hosting. **Take exclusive input** on the active host screen pauses guest keyboard, mouse, and controller input while video continues. Use **Allow guest input** to restore control.

For couch co-op, enable **Guest controller only** in Host options before starting. The guest can use a physical or touch controller while keyboard, mouse, and typed text stay under host control.

Use the stream overlay to distinguish network RTT from host encoding time and client decoding time. A 60 FPS stream has about 16.7 ms per frame. If encoding consistently exceeds that budget, try a lower capture resolution before raising bitrate. If the relay RTT is the bottleneck, use a VmaX relay near both devices or let the default direct path probe run.

Choose **Scan QR** on the join screen to read the host's invite from a camera or saved image. VmaX joins as soon as it reads a valid invite. Under **Settings → Advanced**, choose a frame-pacing priority for the next hosted session or adjust how long the next connection retains incomplete video frames. The packet recovery window only affects incomplete frames.

## Smoothness and pacing

VmaX targets a steady cadence, not just a high frame rate. A stream that reports
60 FPS while frames arrive in bunches still reads as stutter, so the pipeline is
built around a fixed deadline grid:

- The host acquires desktop frames **event-driven**: it blocks in
  `AcquireNextFrame` only for the time left before the next display deadline
  minus the measured encode cost. A frame is never polled for after it is
  already too late to encode, and an idle desktop never spins.
- The deadline grid does not accumulate drift. If a slot is missed (a long
  keyframe, a GPU hitch, a paused desktop), the slot is **skipped**, never repaid
  as a burst of catch-up frames.
- The client decodes with `latencyMode: "realtime"` where WebCodecs supports it,
  and presents at most one frame per display refresh with newest-frame-wins, so
  display latency cannot grow.
- A single lost datagram no longer freezes the picture. The decoder conceals it
  and the host's intra-refresh heals it; a hard reset to a keyframe only happens
  after sustained loss or a real decoder stall.
- When the network worker cannot accept another frame, the host skips the
  capture slot instead of encoding a frame that can only be dropped. Dropping an
  encoded frame would break the reference chain and force an intra-refresh.
- **Encoder budget guard:** if encode time stays above one frame interval, the
  capture/encode size is reduced in coarse steps until the cadence fits, then
  restored after the host has headroom again. Frame rate wins over pixel count
  on slower hardware; the configured stream size is unchanged for any host that
  keeps up.
- The client reports its real display refresh rate, and the host never encodes
  faster than the client can present.

Defaults favor stability on unknown networks: adaptive bitrate (the value in
settings is the ceiling, congestion feedback may lower it), a 32 ms packet
recovery window for incomplete frames only, and 1080p60 at 20 Mbps. Choose
**Fixed bitrate** or a higher target in Settings when the link is known-good.
The in-stream details panel shows **Frame pacing** as the median interval
between presented frames plus its 95th-percentile deviation; healthy 60 Hz
playback sits near `16.67 ms ± <1 ms`.

## Image quality

Smoothness and clarity are set by separate parts of the pipeline, so VmaX tunes
both explicitly instead of trusting encoder defaults.

Quality on every encode path is held to the same rules as the NVENC path:

- **Variable bitrate with a peak above the average.** The average is the number
  in Settings; a single busy frame (a scroll, a cutscene, a window drag) may
  spend up to twice that, and the next quiet frame repays it. Plain CBR clips
  exactly those frames to `bitrate / frame rate`, which is what makes motion look
  smeared even when the reported bitrate is high.
- **Short-term buffer, no reordering.** No B-frames and no lookahead on any
  path: a two-frame rate buffer still caps every frame, without adding a
  decoder-side queue.
- **A long GOP with cheap recovery.** Keyframes are about two seconds apart
  instead of the vendor default. Periodic I-frames do not help a progressive
  stream; they only spike the bitrate, so the client requests a keyframe
  whenever it actually needs one.
- **Quality-leaning encoder effort.** Detail pacing selects the slower search
  (NVENC P3, higher `QualityVsSpeed` on Media Foundation encoders). CABAC is
  enabled for H.264, worth roughly 10% bitrate at the same quality.
- **Priority order in a squeeze:** frame rate first, then resolution, then
  quality. A stutter is never the answer, and a soft frame is always preferable
  to a dropped one.

Colour is carried as **BT.709 full range** end to end, signalled in the
bitstream VUI, so dark themes keep their blacks and highlights are not clipped.
Chroma is averaged over 2x2 blocks rather than point-sampled, which keeps red
and blue text from fringing into neighbouring pixels.

The client renders at the size the video actually occupies in device pixels,
resamples exactly once, and then applies a single contrast-adaptive sharpen
pass. Nothing is stretched twice, so the picture does not soften on a
high-density phone display.

## Friends and saved networks

Open **Friends → Manage friends** to name and save signaling endpoints. Use the default endpoint for local Wi-Fi, or save a permanent public `wss://` endpoint for internet sessions.

1. Join once with a normal QUIC invite. Open **Friends** in the stream controls or on the hosting screen, then add the connected person.
2. On the Windows host, enable **Allow this friend to reconnect** and **Enable friends-only hosting**. From the hosting screen, choose **Switch to friends-only hosting** to close the invite and start accepting approved friends.
3. On the client, open **Friends** and choose **Connect** or **Reconnect**. Enable **Connect automatically when this app is open** to retry while the host is unavailable.

Friends who have paired can choose **Request to join** before the host starts streaming. When the host accepts, VmaX enables reconnect permission and starts friends-only hosting when the host is free. The requester joins automatically while waiting in VmaX and can disconnect and rejoin later. The host can revoke permission under **Manage friends**.

Use an updated VmaX relay for requests across networks. The relay holds pending requests in memory for up to 24 hours, and restarts clear them. Keep the host PC awake with VmaX open to receive and accept requests.

VmaX verifies the saved device certificates on each reconnect. Removing approval ends an active saved-host session and prevents that client from reconnecting. **Disconnect** pauses automatic connections until you resume them or reopen VmaX. Internet reconnects require an updated VmaX signaling server at the same permanent public WSS address used for pairing. The home PC must stay awake with VmaX open. The phone retries while VmaX is open and tries immediately when the app returns to the foreground or the OS reports a network connection.

The live stream uses a two-frame outbound video queue, a bounded audio queue, and a playback buffer capped at 50 ms or one audio-device callback, whichever is larger. When video falls behind, VmaX drops stale deltas and requests a fresh keyframe so playback can catch up.

## Remote controllers

Install the ViGEmBus driver on the Windows host. [ViGEmBus release](https://github.com/nefarius/ViGEmBus/releases/tag/v1.22.0)

Connect an XInput-compatible controller to the Windows client and focus the stream window. VmaX reads the controller in Rust and forwards it as a virtual Xbox 360 controller on the host.

On a touchscreen, open the stream toolbar with the top chevron and tap the gamepad icon to show the touch controller. It has two joysticks, a D-pad, face buttons, bumpers, triggers, stick clicks, View, and Menu. The on-screen controls use the same virtual Xbox 360 controller on the host.

Open **Edit controller layout** in the stream toolbar to move, resize, or hide controls, adjust opacity, remap buttons, or swap the joysticks. **Save** applies the layout and stores it on that client.

# Integrated internet and controller setup

On the Windows host, select **Start Hosting** and share the full
invite with the Windows or Android client. VmaX starts its
own loopback signaling relay and manages the bundled Cloudflare helper. Direct
QUIC remains preferred; encrypted QUIC packets use the WSS relay when necessary.
**Always use the relay** is available for networks where direct probing fails.
Keep VmaX open. After restarting it, start hosting again and share the
fresh invite. Temporary tunnels are for testing and carry no uptime guarantee;
a permanent public relay remains the production option. No protocol can promise
connectivity where a network blocks both direct traffic and the relay service.

Use **Check controller driver** on the host. **Install controller driver** runs
the bundled official, signed ViGEmBus 1.22.0 installer;
Windows requests elevation. VmaX retries driver failures every five
seconds during controller input, releases the virtual controller on disconnect,
and repeats neutral reports so a lost release does not remain held. Connect the
client controller before launching a game that enumerates controllers at startup.
Windows clients use XInput; Android currently provides the on-screen gamepad.
ViGEmBus is an archived dependency, not a new maintained VmaX driver. Physical
Android gamepad mapping, rumble, and multiple simultaneous controllers remain
outside this implementation. Some games may reject virtual controllers.

## Account devices and invites

Apply both SQL migrations in `supabase/migrations` to your Supabase project. Enable email/password sign-in and, for private Realtime channels, disable **Allow public access** in the Realtime settings.

Open **Accounts** and enter the Supabase project URL and publishable key. You can also set `VMAX_SUPABASE_URL` and `VMAX_SUPABASE_PUBLISHABLE_KEY` at build time or place a `supabase.json` file in VmaX's app-local data directory on each device:

```json
{
  "url": "https://YOUR_PROJECT.supabase.co",
  "publishable_key": "sb_publishable_YOUR_KEY"
}
```

Use the publishable key from the project's Connect dialog. Sign in under **Accounts**, pick a username and four-digit tag, then enable **Background hosting** on the Windows PC with a public VmaX `wss://` relay selected. Devices on the same account can use **Instant Self-Connect**. Friends request access by default, and each host can enable unattended access for an accepted friend.

Windows uses DPAPI to protect the saved login and restore the background host settings at sign-in. **Settings → General → Start minimized to tray on boot** controls desktop autostart. A Windows host needs an active Windows user session. VmaX resumes capture after a Windows secure-desktop prompt returns control to the normal desktop.

